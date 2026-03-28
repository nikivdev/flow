//! AI session management for Claude Code, Codex, and Cursor integration.
//!
//! Tracks and manages AI coding sessions per project, allowing users to:
//! - List sessions for the current project (Claude, Codex, or both)
//! - Save/bookmark sessions with names
//! - Resume sessions
//! - Add notes to sessions
//! - Copy session history to clipboard

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use regex::Regex;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use toml::Value as TomlValue;
use tracing::debug;
use uuid::Uuid;

use crate::activity_log;
use crate::cli::{
    AiAction, CodexAgentAction, CodexDaemonAction, CodexMemoryAction, CodexProjectAiAction,
    CodexRuntimeAction, CodexSkillEvalAction, CodexSkillSourceAction, CodexTelemetryAction,
    CodexTraceAction, ProviderAiAction,
};
use crate::commit::configured_codex_bin_for_workdir;
use crate::env as flow_env;
use crate::{
    ai_project_manifest, codex_memory, codex_session_docs, codex_session_index, codex_telemetry,
    codex_text, codexd, config, project_snapshot, repo_capsule, url_inspect,
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

const FLOW_CODEX_SESSION_REPORT_PATH_ENV: &str = "FLOW_ZED_CODEX_SESSION_REPORT_PATH";
const CODEX_SESSION_REPORT_PENDING: &str = "__FLOW_ZED_CODEX_SESSION_PENDING__";
const CODEX_SESSION_REPORT_POLL_LIMIT: usize = 8;
const CODEX_SESSION_REPORT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CODEX_SESSION_REPORT_POLL_TIMEOUT: Duration = Duration::from_secs(15);

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
    #[serde(default)]
    pub(crate) rollout_path: Option<String>,
    pub(crate) updated_at: i64,
    pub(crate) cwd: String,
    pub(crate) title: Option<String>,
    pub(crate) first_user_message: Option<String>,
    pub(crate) git_branch: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) reasoning_effort: Option<String>,
}

#[derive(Debug, Serialize)]
struct CodexRecoverCandidate {
    id: String,
    updated_at: String,
    updated_at_unix: i64,
    cwd: String,
    git_branch: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
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

#[derive(Debug, Serialize)]
struct CodexFindOutput {
    target_path: Option<String>,
    exact_cwd: bool,
    query: String,
    recent_days: Option<u32>,
    all_history: bool,
    selected_session_id: Option<String>,
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
    trace: Option<CodexResolveWorkflowTrace>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexResolveReferenceSnapshot {
    pub name: String,
    pub source: String,
    pub matched: String,
    pub command: Option<String>,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexResolveRuntimeSkillSnapshot {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub trigger: String,
    pub source: Option<String>,
    pub original_name: Option<String>,
    pub estimated_chars: Option<usize>,
    pub match_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexResolveInspectorResponse {
    pub action: String,
    pub route: String,
    pub reason: String,
    pub target_path: String,
    pub launch_path: String,
    pub query: Option<String>,
    pub session_id: Option<String>,
    pub prompt: Option<String>,
    pub references: Vec<CodexResolveReferenceSnapshot>,
    pub runtime_state_path: Option<String>,
    pub runtime_skills: Vec<CodexResolveRuntimeSkillSnapshot>,
    pub prompt_context_budget_chars: usize,
    pub max_resolved_references: usize,
    pub prompt_chars: usize,
    pub injected_context_chars: usize,
    pub trace: Option<CodexResolveWorkflowTrace>,
    pub workflow: Option<CodexResolveWorkflowExplanation>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexResolveWorkflowExplanation {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub trigger: String,
    pub generated_by: String,
    pub packet: CodexResolveWorkflowPacket,
    pub commands: Vec<CodexResolveWorkflowCommand>,
    pub artifacts: Vec<CodexResolveWorkflowArtifact>,
    pub steps: Vec<CodexResolveWorkflowStep>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexResolveWorkflowPacket {
    pub kind: String,
    pub compact_summary: String,
    pub default_view: String,
    pub expansion_rules: Vec<String>,
    pub validation_plan: Vec<CodexResolveWorkflowValidation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<CodexResolveWorkflowTrace>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexResolveWorkflowTrace {
    pub trace_id: String,
    pub span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    pub workflow_kind: String,
    pub service_name: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexResolveWorkflowValidation {
    pub label: String,
    pub tier: String,
    pub detail: String,
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexResolveWorkflowCommand {
    pub label: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexResolveWorkflowArtifact {
    pub label: String,
    pub value: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexResolveWorkflowStep {
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexSessionReferenceRequest {
    session_hints: Vec<String>,
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
pub(crate) const CODEX_FIND_DEFAULT_RECENT_DAYS: u32 = 7;
const CODEX_FIND_RECENT_DAY_SECS: i64 = 24 * 60 * 60;
const CODEX_FIND_RECENT_MONTH_SECS: i64 = 30 * 24 * 60 * 60;
const CODEX_FIND_STRONG_MATCH_SCORE: i64 = 100;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CodexFindScope {
    #[serde(default)]
    pub(crate) recent_days: Option<u32>,
    #[serde(default)]
    pub(crate) all_history: bool,
}

impl Default for CodexFindScope {
    fn default() -> Self {
        Self {
            recent_days: Some(CODEX_FIND_DEFAULT_RECENT_DAYS),
            all_history: false,
        }
    }
}

impl CodexFindScope {
    fn from_cli(recent_days: Option<u32>, all_history: bool) -> Self {
        if all_history {
            Self {
                recent_days: None,
                all_history: true,
            }
        } else {
            Self {
                recent_days,
                all_history: false,
            }
        }
    }

    pub(crate) fn effective_recent_days(self) -> Option<u32> {
        if self.all_history {
            None
        } else {
            Some(self.recent_days.unwrap_or(CODEX_FIND_DEFAULT_RECENT_DAYS))
        }
    }

    pub(crate) fn recent_cutoff_unix(self, now_unix: i64) -> Option<i64> {
        self.effective_recent_days()
            .map(|days| now_unix.saturating_sub((days as i64) * CODEX_FIND_RECENT_DAY_SECS))
    }
}
const CODEX_QUERY_CACHE_ENV_DISABLE: &str = "FLOW_DISABLE_CODEX_QUERY_CACHE";
const CODEX_SESSION_COMPLETION_DEFAULT_SCAN_LIMIT: usize = 24;
const CODEX_SESSION_COMPLETION_DEFAULT_IDLE_SECS: u64 = 90;
const FLOW_CODEX_TRACE_SERVICE_NAME: &str = "flow_codex";
const RUN_AGENT_ROUTER_PATH: &str = "~/run/scripts/agent-router.sh";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexThreadSchema {
    has_rollout_path: bool,
    has_model: bool,
    has_reasoning_effort: bool,
}

#[derive(Debug, Clone)]
struct CodexThreadSchemaCacheEntry {
    stamp: CodexStateDbStamp,
    schema: CodexThreadSchema,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexSessionCompletionSnapshot {
    last_role: Option<String>,
    last_user_message: Option<String>,
    last_user_at_unix: Option<u64>,
    last_assistant_message: Option<String>,
    last_assistant_at_unix: Option<u64>,
    file_modified_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexTurnPatchChange {
    path: String,
    action: String,
    patch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrFeedbackCursorHandoff {
    workspace_path: PathBuf,
    review_plan_path: PathBuf,
    review_rules_path: Option<PathBuf>,
    kit_system_path: PathBuf,
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
            Some(ProviderAiAction::Eval { .. }) => {
                bail!("eval is only supported for Codex sessions; use `f codex eval`");
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
            Some(ProviderAiAction::Telemetry { .. }) => {
                bail!(
                    "telemetry is only supported for Codex sessions; use `f codex telemetry ...`"
                );
            }
            Some(ProviderAiAction::Trace { .. }) => {
                bail!("trace is only supported for Codex sessions; use `f codex trace ...`");
            }
            Some(ProviderAiAction::ProjectAi { .. }) => {
                bail!(
                    "project-ai is only supported for Codex sessions; use `f codex project-ai ...`"
                );
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
            Some(ProviderAiAction::Agent { .. }) => {
                bail!("agent is only supported for Codex sessions; use `f codex agent ...`");
            }
            Some(ProviderAiAction::Sessions { .. })
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
        Some(ProviderAiAction::Sessions { path, json }) => provider_sessions(provider, path, json)?,
        Some(ProviderAiAction::Continue { session, path }) => {
            continue_session(session, path, provider)?
        }
        Some(ProviderAiAction::New) => new_session(provider)?,
        Some(ProviderAiAction::Connect {
            path,
            exact_cwd,
            json,
            recent_days,
            all_history,
            query,
        }) => connect_codex_session(
            path,
            query,
            exact_cwd,
            json,
            CodexFindScope::from_cli(recent_days, all_history),
            provider,
        )?,
        Some(ProviderAiAction::Open {
            path,
            exact_cwd,
            query,
        }) => open_codex_session(path, query, exact_cwd, provider)?,
        Some(ProviderAiAction::Daemon { action }) => codex_daemon_command(action, provider)?,
        Some(ProviderAiAction::Memory { action }) => codex_memory_command(action, provider)?,
        Some(ProviderAiAction::Telemetry { action }) => codex_telemetry_command(action, provider)?,
        Some(ProviderAiAction::Trace { action }) => codex_trace_command(action, provider)?,
        Some(ProviderAiAction::ProjectAi { action }) => codex_project_ai_command(action, provider)?,
        Some(ProviderAiAction::SkillEval { action }) => codex_skill_eval_command(action, provider)?,
        Some(ProviderAiAction::SkillSource { action }) => {
            codex_skill_source_command(action, provider)?
        }
        Some(ProviderAiAction::Agent { action }) => codex_agent_command(action, provider)?,
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
        Some(ProviderAiAction::Eval { path, limit, json }) => {
            codex_eval(path, limit, json, provider)?
        }
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
            json,
            limit,
            recent_days,
            all_history,
            query,
        }) => find_codex_session(
            path,
            query,
            exact_cwd,
            json,
            limit,
            CodexFindScope::from_cli(recent_days, all_history),
            provider,
        )?,
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
            Some(ProviderAiAction::Sessions { path, json }) => {
                provider_sessions(Provider::Claude, path, json)?
            }
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
            Some(ProviderAiAction::Eval { .. }) => {
                bail!("eval is only supported for Codex sessions; use `f codex eval`");
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
            Some(ProviderAiAction::Telemetry { .. }) => {
                bail!(
                    "telemetry is only supported for Codex sessions; use `f codex telemetry ...`"
                );
            }
            Some(ProviderAiAction::Trace { .. }) => {
                bail!("trace is only supported for Codex sessions; use `f codex trace ...`");
            }
            Some(ProviderAiAction::ProjectAi { .. }) => {
                bail!(
                    "project-ai is only supported for Codex sessions; use `f codex project-ai ...`"
                );
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
            Some(ProviderAiAction::Agent { .. }) => {
                bail!("agent is only supported for Codex sessions; use `f codex agent ...`");
            }
            Some(ProviderAiAction::Resume { session, path }) => {
                resume_session(session, path, Provider::Claude)?
            }
            Some(ProviderAiAction::Find {
                path,
                exact_cwd,
                json,
                limit,
                recent_days,
                all_history,
                query,
            }) => find_codex_session(
                path,
                query,
                exact_cwd,
                json,
                limit,
                CodexFindScope::from_cli(recent_days, all_history),
                Provider::Claude,
            )?,
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
            Some(ProviderAiAction::Sessions { path, json }) => {
                provider_sessions(Provider::Codex, path, json)?
            }
            Some(ProviderAiAction::Continue { session, path }) => {
                continue_session(session, path, Provider::Codex)?
            }
            Some(ProviderAiAction::New) => new_session(Provider::Codex)?,
            Some(ProviderAiAction::Connect {
                path,
                exact_cwd,
                json,
                recent_days,
                all_history,
                query,
            }) => connect_codex_session(
                path,
                query,
                exact_cwd,
                json,
                CodexFindScope::from_cli(recent_days, all_history),
                Provider::Codex,
            )?,
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
            Some(ProviderAiAction::Telemetry { action }) => {
                codex_telemetry_command(action, Provider::Codex)?
            }
            Some(ProviderAiAction::Trace { action }) => {
                codex_trace_command(action, Provider::Codex)?
            }
            Some(ProviderAiAction::ProjectAi { action }) => {
                codex_project_ai_command(action, Provider::Codex)?
            }
            Some(ProviderAiAction::SkillEval { action }) => {
                codex_skill_eval_command(action, Provider::Codex)?
            }
            Some(ProviderAiAction::SkillSource { action }) => {
                codex_skill_source_command(action, Provider::Codex)?
            }
            Some(ProviderAiAction::Agent { action }) => {
                codex_agent_command(action, Provider::Codex)?
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
            Some(ProviderAiAction::Eval { path, limit, json }) => {
                codex_eval(path, limit, json, Provider::Codex)?
            }
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
                json,
                limit,
                recent_days,
                all_history,
                query,
            }) => find_codex_session(
                path,
                query,
                exact_cwd,
                json,
                limit,
                CodexFindScope::from_cli(recent_days, all_history),
                Provider::Codex,
            )?,
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

fn keep_from_heading(text: &str, heading: &str) -> String {
    let mut offset = 0usize;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with(heading) {
            let suffix = trimmed[heading.len()..].trim_start_matches(':').trim();
            let mut result = String::new();
            if !suffix.is_empty() {
                result.push_str(suffix);
            }
            let remainder_start = offset + line.len();
            let remainder = text[remainder_start..].trim_start_matches('\n').trim();
            if !remainder.is_empty() {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(remainder);
            }
            return result.trim().to_string();
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
    if role == "user" && cleaned.contains("User request:") {
        cleaned = keep_from_heading(&cleaned, "User request:");
    }

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

fn ai_session_from_codex_recover_row(row: CodexRecoverRow) -> AiSession {
    let updated_at = DateTime::<Utc>::from_timestamp(row.updated_at, 0)
        .map(|value| value.to_rfc3339())
        .unwrap_or_else(|| row.updated_at.to_string());
    let title = row.title.filter(|value| !value.trim().is_empty());
    let first_user_message = row
        .first_user_message
        .filter(|value| !value.trim().is_empty());
    let last_message = title.clone().or_else(|| first_user_message.clone());

    AiSession {
        session_id: row.id,
        provider: Provider::Codex,
        timestamp: Some(updated_at.clone()),
        last_message_at: Some(updated_at),
        last_message,
        first_message: first_user_message,
        error_summary: None,
    }
}

fn read_codex_sessions_for_path_from_files(path: &PathBuf) -> Result<Vec<AiSession>> {
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

fn read_codex_sessions_for_path(path: &PathBuf) -> Result<Vec<AiSession>> {
    let db_result = (|| -> Result<Vec<AiSession>> {
        let db_path = codex_state_db_path()?;
        let schema = load_codex_thread_schema(&db_path)?;
        let target = path.to_string_lossy().to_string();
        let cache_key = format!("target={target}");
        let sql = format!(
            r#"
{}
where archived = 0
  and cwd = ?1
order by updated_at desc
"#,
            codex_recover_select_sql(&schema)
        );

        let rows = with_codex_query_cache(&db_path, "session-list-exact", &cache_key, |conn| {
            let mut stmt = conn
                .prepare(&sql)
                .context("failed to prepare codex session list query")?;
            let iter = stmt.query_map(params![target], map_codex_recover_row)?;
            Ok(iter.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;

        Ok(rows
            .into_iter()
            .map(ai_session_from_codex_recover_row)
            .collect())
    })();

    match db_result {
        Ok(sessions) => Ok(sessions),
        Err(err) => {
            debug!(
                error = %err,
                path = %path.display(),
                "failed to read codex sessions from state db; falling back to session files"
            );
            read_codex_sessions_for_path_from_files(path)
        }
    }
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

#[derive(Debug, Serialize)]
struct ProviderSessionListRow {
    index: usize,
    id: String,
    updated_at: Option<String>,
    updated_relative: String,
    preview: String,
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

    let has_tty = io::stdin().is_terminal() && io::stdout().is_terminal();

    // Check for interactive selection support.
    if !has_tty || which::which("fzf").is_err() {
        if !has_tty {
            println!("Interactive selection unavailable without a TTY.");
        } else {
            println!("fzf not found – install it for fuzzy selection.");
        }
        println!("\nSessions:");
        for entry in &entries {
            println!("{}", entry.display);
        }
        if !has_tty {
            println!(
                "\nTip: use `f ai codex sessions --path <repo>` for machine-readable selection."
            );
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
    launch_session_for_target(session_id, provider, None, None, None, None)
}

fn new_codex_session_trace(workflow_kind: &str) -> CodexResolveWorkflowTrace {
    CodexResolveWorkflowTrace {
        trace_id: new_workflow_trace_id(),
        span_id: new_workflow_span_id(),
        parent_span_id: None,
        workflow_kind: workflow_kind.to_string(),
        service_name: FLOW_CODEX_TRACE_SERVICE_NAME.to_string(),
    }
}

fn direct_codex_trace_query(action: &str, route: &str, session_id: Option<&str>) -> String {
    match route {
        "continue-last-direct" => "continue last codex session".to_string(),
        "new-direct" => "start new codex session".to_string(),
        "resume-direct" if session_id.is_some() => {
            format!(
                "resume codex session {}",
                truncate_recover_id(session_id.unwrap_or_default())
            )
        }
        "resume-direct" => "resume codex session".to_string(),
        "connect-direct" if session_id.is_some() => {
            format!(
                "connect codex session {}",
                truncate_recover_id(session_id.unwrap_or_default())
            )
        }
        "connect-direct" => "connect codex session".to_string(),
        _ => format!("{action} codex session"),
    }
}

fn record_direct_codex_launch_event(
    action: &str,
    route: &str,
    target_path: &Path,
    launch_path: &Path,
    session_id: Option<&str>,
    trace: &CodexResolveWorkflowTrace,
) {
    let query = direct_codex_trace_query(action, route, session_id);
    let event = codex_skill_eval::CodexSkillEvalEvent {
        version: 1,
        recorded_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_secs())
            .unwrap_or(0),
        mode: "direct-launch".to_string(),
        action: action.to_string(),
        route: route.to_string(),
        target_path: target_path.display().to_string(),
        launch_path: launch_path.display().to_string(),
        query: query.clone(),
        session_id: session_id.map(str::to_string),
        runtime_token: None,
        runtime_skills: Vec::new(),
        prompt_context_budget_chars: 0,
        prompt_chars: query.chars().count(),
        injected_context_chars: 0,
        reference_count: 0,
        trace_id: Some(trace.trace_id.clone()),
        span_id: Some(trace.span_id.clone()),
        parent_span_id: trace.parent_span_id.clone(),
        workflow_kind: Some(trace.workflow_kind.clone()),
        service_name: Some(trace.service_name.clone()),
    };
    let _ = codex_skill_eval::log_event(&event);
}

#[derive(Debug, Default)]
struct CodexSessionReportBaseline {
    started_at_unix: i64,
    exact_ids: BTreeSet<String>,
    tree_ids: BTreeSet<String>,
}

fn codex_session_report_path_from_env() -> Option<PathBuf> {
    env::var(FLOW_CODEX_SESSION_REPORT_PATH_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn write_codex_session_report(path: &Path, session_id: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, format!("{}\n", session_id.trim()))
        .with_context(|| format!("failed to write {}", path.display()))
}

fn clear_pending_codex_session_report(path: &Path) {
    let Ok(current) = fs::read_to_string(path) else {
        return;
    };
    if current.trim() != CODEX_SESSION_REPORT_PENDING {
        return;
    }
    if let Err(err) = fs::remove_file(path) {
        debug!(
            error = %err,
            path = %path.display(),
            "failed to remove pending Codex session report"
        );
    }
}

fn maybe_write_codex_session_report(session_id: &str) {
    let Some(report_path) = codex_session_report_path_from_env() else {
        return;
    };

    if let Err(err) = write_codex_session_report(&report_path, session_id) {
        debug!(
            error = %err,
            path = %report_path.display(),
            "failed to write Codex session report"
        );
    }
}

fn capture_codex_session_report_baseline(target_path: &Path) -> CodexSessionReportBaseline {
    let exact_ids =
        read_recent_codex_threads_local(target_path, true, CODEX_SESSION_REPORT_POLL_LIMIT, None)
            .unwrap_or_default()
            .into_iter()
            .map(|row| row.id)
            .collect();
    let tree_ids =
        read_recent_codex_threads_local(target_path, false, CODEX_SESSION_REPORT_POLL_LIMIT, None)
            .unwrap_or_default()
            .into_iter()
            .map(|row| row.id)
            .collect();

    CodexSessionReportBaseline {
        started_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or_default(),
        exact_ids,
        tree_ids,
    }
}

fn find_new_codex_session_report_id(
    target_path: &Path,
    baseline: &CodexSessionReportBaseline,
) -> Option<String> {
    let min_updated_at = baseline.started_at_unix.saturating_sub(2);

    let exact_rows =
        read_recent_codex_threads_local(target_path, true, CODEX_SESSION_REPORT_POLL_LIMIT, None)
            .ok()?;
    if let Some(row) = exact_rows.into_iter().find(|row| {
        row.updated_at >= min_updated_at && !baseline.exact_ids.contains(row.id.as_str())
    }) {
        return Some(row.id);
    }

    let tree_rows =
        read_recent_codex_threads_local(target_path, false, CODEX_SESSION_REPORT_POLL_LIMIT, None)
            .ok()?;
    tree_rows
        .into_iter()
        .find(|row| {
            row.updated_at >= min_updated_at && !baseline.tree_ids.contains(row.id.as_str())
        })
        .map(|row| row.id)
}

fn start_new_codex_session_reporter(report_path: PathBuf, target_path: PathBuf) {
    let baseline = capture_codex_session_report_baseline(&target_path);
    thread::spawn(move || {
        let started_at = Instant::now();
        while started_at.elapsed() < CODEX_SESSION_REPORT_POLL_TIMEOUT {
            if let Some(session_id) = find_new_codex_session_report_id(&target_path, &baseline) {
                if let Err(err) = write_codex_session_report(&report_path, &session_id) {
                    debug!(
                        error = %err,
                        path = %report_path.display(),
                        "failed to write new Codex session report"
                    );
                }
                return;
            }

            thread::sleep(CODEX_SESSION_REPORT_POLL_INTERVAL);
        }

        clear_pending_codex_session_report(&report_path);
        debug!(
            path = %report_path.display(),
            target_path = %target_path.display(),
            "timed out while waiting for a new Codex session id to appear"
        );
    });
}

fn launch_session_for_target(
    session_id: &str,
    provider: Provider,
    prompt: Option<&str>,
    target_path: Option<&Path>,
    runtime_state_path: Option<&str>,
    trace: Option<&CodexResolveWorkflowTrace>,
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
            let direct_log = trace.is_none();
            let effective_trace = trace
                .cloned()
                .unwrap_or_else(|| new_codex_session_trace("resume_session"));
            let mut command = Command::new(configured_codex_bin_for_workdir(&workdir));
            command.arg("resume");
            if let Some(path) = target_path {
                command.current_dir(path);
            }
            apply_codex_personal_env_to_command(&mut command);
            apply_codex_trust_overrides_for(&mut command, target_path);
            apply_codex_runtime_state_to_command(&mut command, runtime_state_path);
            apply_codex_trace_env_to_command(&mut command, Some(&effective_trace));
            command
                .arg("--dangerously-bypass-approvals-and-sandbox")
                .arg(session_id);
            if let Some(prompt) = prompt.map(str::trim).filter(|value| !value.is_empty()) {
                command.arg(prompt);
            }
            maybe_write_codex_session_report(session_id);
            let status = command.status().with_context(|| "failed to launch codex")?;
            if status.success() && direct_log {
                record_direct_codex_launch_event(
                    "resume",
                    "resume-direct",
                    &workdir,
                    &workdir,
                    Some(session_id),
                    &effective_trace,
                );
            }
            status
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

fn run_agent_router_path() -> PathBuf {
    config::expand_path(RUN_AGENT_ROUTER_PATH)
}

fn parse_run_agent_list_output(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn command_output_error_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }
    format!("exit status {}", output.status)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexRunAgentBridgeStatus {
    router_path: String,
    status: String,
    agent_count: usize,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
struct CodexRunAgentHandoff {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    next_action: String,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    relevant_paths: Vec<String>,
    #[serde(default)]
    validation: Vec<String>,
    #[serde(default)]
    open_questions: Vec<String>,
    #[serde(default)]
    source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
struct CodexRunAgentCompletedEvent {
    #[serde(rename = "type", default)]
    event_type: String,
    #[serde(default)]
    agent_id: String,
    #[serde(default)]
    invocation_id: String,
    #[serde(default)]
    artifact_path: Option<String>,
    #[serde(default)]
    handoff: Option<CodexRunAgentHandoff>,
    #[serde(default)]
    output: String,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    trace_path: Option<String>,
}

fn collect_run_agent_bridge_status() -> CodexRunAgentBridgeStatus {
    let router_path = run_agent_router_path();
    let router_path_display = router_path.display().to_string();
    if !router_path.is_file() {
        return CodexRunAgentBridgeStatus {
            router_path: router_path_display,
            status: "missing".to_string(),
            agent_count: 0,
            error: Some("router script not found".to_string()),
        };
    }

    match Command::new("bash").arg(&router_path).arg("list").output() {
        Ok(output) if output.status.success() => {
            let agent_ids = parse_run_agent_list_output(&output.stdout);
            CodexRunAgentBridgeStatus {
                router_path: router_path_display,
                status: "ready".to_string(),
                agent_count: agent_ids.len(),
                error: None,
            }
        }
        Ok(output) => CodexRunAgentBridgeStatus {
            router_path: router_path_display,
            status: "error".to_string(),
            agent_count: 0,
            error: Some(command_output_error_detail(&output)),
        },
        Err(err) => CodexRunAgentBridgeStatus {
            router_path: router_path_display,
            status: "error".to_string(),
            agent_count: 0,
            error: Some(err.to_string()),
        },
    }
}

fn require_run_agent_router_path() -> Result<PathBuf> {
    let router_path = run_agent_router_path();
    if !router_path.is_file() {
        bail!(
            "run-agent bridge is unavailable; expected router at {}",
            router_path.display()
        );
    }
    Ok(router_path)
}

fn run_agent_router_list() -> Result<Vec<String>> {
    let router_path = require_run_agent_router_path()?;
    let output = Command::new("bash")
        .arg(&router_path)
        .arg("list")
        .output()
        .with_context(|| format!("failed to execute {}", router_path.display()))?;
    if !output.status.success() {
        bail!(
            "run-agent bridge list failed: {}",
            command_output_error_detail(&output)
        );
    }
    Ok(parse_run_agent_list_output(&output.stdout))
}

fn run_agent_router_show(agent_id: &str) -> Result<String> {
    let router_path = require_run_agent_router_path()?;
    let output = Command::new("bash")
        .arg(&router_path)
        .arg("show")
        .arg(agent_id)
        .output()
        .with_context(|| format!("failed to execute {}", router_path.display()))?;
    if !output.status.success() {
        bail!(
            "run-agent bridge show failed: {}",
            command_output_error_detail(&output)
        );
    }
    String::from_utf8(output.stdout).context("failed to decode run-agent show output")
}

fn parse_run_agent_completed_event(stdout: &[u8]) -> Result<CodexRunAgentCompletedEvent> {
    let rendered = String::from_utf8(stdout.to_vec())
        .context("failed to decode run-agent event stream as UTF-8")?;
    let mut completed: Option<CodexRunAgentCompletedEvent> = None;

    for line in rendered.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(trimmed).context("failed to parse run-agent event JSON")?;
        if value.get("type").and_then(|item| item.as_str()) == Some("completed") {
            completed = Some(
                serde_json::from_value(value)
                    .context("failed to decode run-agent completed event")?,
            );
        }
    }

    completed.context("run-agent bridge returned no completed event")
}

fn run_agent_bridge_completed_event_summary(event: &CodexRunAgentCompletedEvent) -> String {
    let output = event.output.trim();
    if !output.is_empty() {
        return truncate_message(output, 180);
    }
    if let Some(handoff) = &event.handoff {
        let summary = handoff.summary.trim();
        if !summary.is_empty() {
            return truncate_message(summary, 180);
        }
    }
    if !event.agent_id.is_empty() {
        return format!("completed {}", event.agent_id);
    }
    "completed run-agent bridge".to_string()
}

fn run_codex_agent_bridge(
    agent_id: &str,
    target_path: &Path,
    new_thread: bool,
    query_text: &str,
) -> Result<CodexRunAgentCompletedEvent> {
    let router_path = require_run_agent_router_path()?;
    let repo_root = detect_git_root(target_path).unwrap_or_else(|| target_path.to_path_buf());
    let project_name = repo_root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("project")
        .to_string();
    let request = json!({
        "query": query_text,
        "context": {
            "workspace_path": target_path.display().to_string(),
            "workspace_repo_root": repo_root.display().to_string(),
            "project_path": repo_root.display().to_string(),
            "project_name": project_name,
            "entry_harness_id": "flow.codex.agent",
            "input_surface": "flow",
        }
    });
    let request_bytes =
        serde_json::to_vec(&request).context("failed to encode run-agent request payload")?;

    let mut command = Command::new("bash");
    command.arg(&router_path).arg("run-json");
    if new_thread {
        command.arg("--new-thread");
    }
    command
        .arg(agent_id)
        .current_dir(target_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to launch run-agent bridge for `{agent_id}`"))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .context("run-agent bridge stdin was not available")?;
        stdin
            .write_all(&request_bytes)
            .context("failed to write run-agent request payload")?;
        stdin
            .write_all(b"\n")
            .context("failed to terminate run-agent request payload")?;
    }
    let output = child
        .wait_with_output()
        .context("failed to wait for run-agent bridge process")?;
    if !output.status.success() {
        bail!(
            "run-agent bridge run failed: {}",
            command_output_error_detail(&output)
        );
    }
    parse_run_agent_completed_event(&output.stdout)
}

fn record_run_agent_bridge_activity(
    agent_id: &str,
    target_path: &Path,
    repo_root: &Path,
    event: &CodexRunAgentCompletedEvent,
) {
    let mut activity_event = activity_log::ActivityEvent::done(
        "codex.agent.run",
        run_agent_bridge_completed_event_summary(event),
    );
    activity_event.route = Some(format!("agent.{agent_id}"));
    activity_event.scope = Some(agent_id.to_string());
    activity_event.source = Some("run-agent-bridge".to_string());
    activity_event.session_id = event.thread_id.clone();
    activity_event.target_path = Some(repo_root.display().to_string());
    activity_event.launch_path = Some(target_path.display().to_string());
    activity_event.artifact_path = event.artifact_path.clone();
    activity_event.payload_ref = event.trace_path.clone();
    let _ = activity_log::append_daily_event(activity_event);
}

fn print_run_agent_completed_event(event: &CodexRunAgentCompletedEvent) {
    let output = event.output.trim_end();
    let fallback_summary = event
        .handoff
        .as_ref()
        .map(|handoff| handoff.summary.trim())
        .filter(|summary| !summary.is_empty());
    let mut printed_body = false;
    if !output.is_empty() {
        println!("{output}");
        printed_body = true;
    } else if let Some(summary) = fallback_summary {
        println!("{summary}");
        printed_body = true;
    }

    let mut metadata = vec![format!("agent_id: {}", event.agent_id)];
    if let Some(thread_id) = &event.thread_id {
        metadata.push(format!("thread_id: {thread_id}"));
    }
    if let Some(artifact_path) = &event.artifact_path {
        metadata.push(format!("artifact_path: {artifact_path}"));
    }
    if let Some(trace_path) = &event.trace_path {
        metadata.push(format!("trace_path: {trace_path}"));
    }

    if !metadata.is_empty() {
        if printed_body {
            println!();
        }
        for line in metadata {
            println!("{line}");
        }
    }
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

fn apply_codex_trace_env_to_command(
    command: &mut Command,
    trace: Option<&CodexResolveWorkflowTrace>,
) {
    let Some(trace) = trace else {
        return;
    };
    command.env("FLOW_TRACE_ID", &trace.trace_id);
    command.env("FLOW_SPAN_ID", &trace.span_id);
    if let Some(parent_span_id) = trace.parent_span_id.as_deref() {
        command.env("FLOW_PARENT_SPAN_ID", parent_span_id);
    }
    command.env("FLOW_WORKFLOW_KIND", &trace.workflow_kind);
    command.env("FLOW_TRACE_SERVICE_NAME", &trace.service_name);
}

fn codex_personal_env_keys() -> Vec<String> {
    [
        "FLOW_CODEX_MAPLE_LOCAL_ENDPOINT",
        "FLOW_CODEX_MAPLE_LOCAL_INGEST_KEY",
        "FLOW_CODEX_MAPLE_HOSTED_ENDPOINT",
        "FLOW_CODEX_MAPLE_HOSTED_INGEST_KEY",
        "FLOW_CODEX_MAPLE_HOSTED_PUBLIC_INGEST_KEY",
        "FLOW_CODEX_MAPLE_TRACES_ENDPOINTS",
        "FLOW_CODEX_MAPLE_INGEST_KEYS",
        "FLOW_CODEX_MAPLE_SERVICE_NAME",
        "FLOW_CODEX_MAPLE_SERVICE_VERSION",
        "FLOW_CODEX_MAPLE_SCOPE_NAME",
        "FLOW_CODEX_MAPLE_ENV",
        "FLOW_CODEX_MAPLE_QUEUE_CAPACITY",
        "FLOW_CODEX_MAPLE_MAX_BATCH_SIZE",
        "FLOW_CODEX_MAPLE_FLUSH_INTERVAL_MS",
        "FLOW_CODEX_MAPLE_CONNECT_TIMEOUT_MS",
        "FLOW_CODEX_MAPLE_REQUEST_TIMEOUT_MS",
        "MAPLE_API_TOKEN",
        "MAPLE_MCP_URL",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn codex_has_explicit_maple_env() -> bool {
    codex_personal_env_keys().into_iter().any(|key| {
        env::var(&key)
            .ok()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    })
}

fn apply_codex_personal_env_to_command(command: &mut Command) {
    if codex_has_explicit_maple_env() {
        return;
    }
    let missing_keys: Vec<String> = codex_personal_env_keys()
        .into_iter()
        .filter(|key| {
            env::var(key)
                .ok()
                .map(|v| v.trim().is_empty())
                .unwrap_or(true)
        })
        .collect();
    if missing_keys.is_empty() {
        return;
    }
    let Ok(values) = flow_env::fetch_local_personal_env_vars(&missing_keys) else {
        return;
    };
    for (key, value) in values {
        if !value.trim().is_empty() {
            command.env(key, value);
        }
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
    apply_codex_personal_env_to_command(&mut command);
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
    if let Some(session_id) = read_recent_codex_threads(&workdir, true, 1, None)?
        .first()
        .map(|row| row.id.clone())
    {
        maybe_write_codex_session_report(&session_id);
    }
    let trace = new_codex_session_trace("continue_last_session");
    let mut command = Command::new(configured_codex_bin_for_workdir(&workdir));
    command.arg("resume");
    if let Some(path) = target_path {
        command.current_dir(path);
    }
    apply_codex_personal_env_to_command(&mut command);
    apply_codex_trust_overrides_for(&mut command, target_path);
    apply_codex_trace_env_to_command(&mut command, Some(&trace));
    command
        .arg("--last")
        .arg("--dangerously-bypass-approvals-and-sandbox");
    let status = command
        .status()
        .with_context(|| "failed to launch codex resume --last")?;
    if status.success() {
        record_direct_codex_launch_event(
            "resume",
            "continue-last-direct",
            &workdir,
            &workdir,
            None,
            &trace,
        );
    }
    Ok(status.success())
}

fn should_fast_path_codex_connect(query_text: &str, exact_cwd: bool, json_output: bool) -> bool {
    query_text.trim().is_empty() && exact_cwd && !json_output
}

fn record_codex_connect_activity(
    summary: &str,
    route: &str,
    target_path: &Path,
    launch_path: &Path,
    session_id: Option<&str>,
) {
    let mut connect_event = activity_log::ActivityEvent::done("codex.connect", summary);
    connect_event.route = Some(route.to_string());
    connect_event.target_path = Some(target_path.display().to_string());
    connect_event.launch_path = Some(launch_path.display().to_string());
    connect_event.session_id = session_id.map(str::to_string);
    connect_event.source = Some("codex-connect".to_string());
    let _ = activity_log::append_daily_event(connect_event);
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

fn print_provider_session_listing(
    provider: Provider,
    target: &Path,
    sessions: &[AiSession],
    json: bool,
) -> Result<()> {
    if sessions.is_empty() {
        let provider_name = match provider {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Cursor => "Cursor",
            Provider::All => "AI",
        };
        bail!("No {provider_name} sessions found for {}", target.display());
    }

    let rows: Vec<ProviderSessionListRow> = sessions
        .iter()
        .enumerate()
        .map(|(index, session)| {
            let updated_at = session
                .last_message_at
                .clone()
                .or_else(|| session.timestamp.clone());
            let updated_relative = updated_at
                .as_deref()
                .map(format_relative_time)
                .unwrap_or_else(|| "-".to_string());
            let preview = session
                .last_message
                .as_deref()
                .or(session.first_message.as_deref())
                .or(session.error_summary.as_deref())
                .map(clean_summary)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "(no message)".to_string());
            ProviderSessionListRow {
                index: index + 1,
                id: session.session_id.clone(),
                updated_at,
                updated_relative,
                preview,
            }
        })
        .collect();

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).context("failed to encode session list JSON")?
        );
        return Ok(());
    }

    println!(
        "{} sessions for {}",
        provider_name(provider),
        target.display()
    );
    println!();

    let index_width = rows
        .last()
        .map(|row| row.index.to_string().len())
        .unwrap_or(1)
        .max(1);
    let updated_width = rows
        .iter()
        .map(|row| row.updated_relative.chars().count())
        .max()
        .unwrap_or(7)
        .max("updated".len());
    let id_width = rows
        .iter()
        .map(|row| row.id.chars().count())
        .max()
        .unwrap_or(10)
        .min(36)
        .max(2);

    println!(
        "{:>index_width$}  {:<updated_width$}  {:<id_width$}  preview",
        "#",
        "updated",
        "id",
        index_width = index_width,
        updated_width = updated_width,
        id_width = id_width,
    );
    for row in &rows {
        println!(
            "{:>index_width$}  {:<updated_width$}  {:<id_width$}  {}",
            row.index,
            row.updated_relative,
            row.id,
            truncate_str(&row.preview, 90),
            index_width = index_width,
            updated_width = updated_width,
            id_width = id_width,
        );
    }

    println!();
    println!(
        "Continue with `f ai {} continue <index|id-prefix> --path {}`",
        provider_name(provider),
        shell_words::quote(&target.display().to_string())
    );
    Ok(())
}

fn provider_sessions(provider: Provider, path: Option<String>, json: bool) -> Result<()> {
    if provider == Provider::All {
        bail!("sessions requires a specific provider (claude or codex)");
    }
    if provider == Provider::Codex {
        let target = resolve_session_target_path(path.as_deref())?;
        let sessions = read_sessions_for_target(provider, path.as_deref())?;
        return print_provider_session_listing(provider, &target, &sessions, json);
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
        if launch_session_for_target(
            &sess.session_id,
            sess.provider,
            None,
            Some(&target),
            None,
            None,
        )? {
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
    new_session_for_target(provider, None, None, None, None)
}

fn new_session_for_target(
    provider: Provider,
    prompt: Option<&str>,
    target_path: Option<&Path>,
    runtime_state_path: Option<&str>,
    trace: Option<&CodexResolveWorkflowTrace>,
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
            let direct_log = trace.is_none();
            let effective_trace = trace
                .cloned()
                .unwrap_or_else(|| new_codex_session_trace("new_session"));
            let mut command = Command::new(configured_codex_bin_for_workdir(&workdir));
            if let Some(path) = target_path {
                command.current_dir(path);
            }
            apply_codex_personal_env_to_command(&mut command);
            apply_codex_trust_overrides_for(&mut command, target_path);
            apply_codex_runtime_state_to_command(&mut command, runtime_state_path);
            apply_codex_trace_env_to_command(&mut command, Some(&effective_trace));
            command
                .arg("--yolo")
                .arg("--sandbox")
                .arg("danger-full-access");
            if let Some(prompt) = prompt.map(str::trim).filter(|value| !value.is_empty()) {
                command.arg(prompt);
            }
            let report_path = codex_session_report_path_from_env();
            if let Some(report_path) = report_path.clone() {
                if let Err(err) =
                    write_codex_session_report(&report_path, CODEX_SESSION_REPORT_PENDING)
                {
                    debug!(
                        error = %err,
                        path = %report_path.display(),
                        "failed to clear stale Codex session report before new launch"
                    );
                }
                start_new_codex_session_reporter(report_path, workdir.clone());
            }
            let status = command.status().with_context(|| "failed to launch codex")?;
            if !status.success()
                && let Some(report_path) = report_path.as_deref()
            {
                clear_pending_codex_session_report(report_path);
            }
            if status.success() && direct_log {
                record_direct_codex_launch_event(
                    "new",
                    "new-direct",
                    &workdir,
                    &workdir,
                    None,
                    &effective_trace,
                );
            }
            status
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
    json_output: bool,
    limit: usize,
    scope: CodexFindScope,
    provider: Provider,
) -> Result<()> {
    if provider != Provider::Codex {
        bail!("find is only supported for Codex sessions; use `f ai codex find ...`");
    }

    let query_text = normalize_recover_query(&query).ok_or_else(|| {
        anyhow::anyhow!(
            "find requires a query, for example: `f ai codex find \"make plan to get designer\"`"
        )
    })?;
    let target_path = path
        .clone()
        .map(|value| canonicalize_recover_path(Some(value)))
        .transpose()?;
    let rows = search_codex_threads_for_find(
        target_path.as_deref(),
        exact_cwd,
        &query_text,
        limit.max(1),
        scope,
    )?;

    if json_output {
        let output = CodexFindOutput {
            target_path: target_path
                .as_ref()
                .map(|value| value.display().to_string()),
            exact_cwd,
            query: query_text,
            recent_days: scope.effective_recent_days(),
            all_history: scope.all_history,
            selected_session_id: rows.first().map(|row| row.id.clone()),
            candidates: rows_to_recover_candidates(rows),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&output).context("failed to encode Codex find JSON")?
        );
        return Ok(());
    }

    let selected = rows.first().ok_or_else(|| match target_path.as_ref() {
        Some(target_path) => anyhow::anyhow!(
            "No matching Codex sessions found for {:?} under {}",
            query_text,
            target_path.display()
        ),
        None => anyhow::anyhow!("No matching Codex sessions found for {:?}", query_text),
    })?;
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
    resume_session(Some(selected.id.clone()), None, Provider::Codex)
}

fn find_and_copy_codex_session(
    path: Option<String>,
    query: Vec<String>,
    exact_cwd: bool,
    provider: Provider,
) -> Result<()> {
    let selected = find_best_codex_session_match(
        path,
        query,
        exact_cwd,
        CodexFindScope::default(),
        provider,
        "findAndCopy",
        false,
    )?;
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
    scope: CodexFindScope,
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
    let rows =
        search_codex_threads_for_find(target_path.as_deref(), exact_cwd, &query_text, 5, scope)?;
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

fn extract_codex_session_hints(query: &str) -> Vec<String> {
    let mut hints = Vec::new();
    for token in recover_query_tokens(query) {
        if looks_like_git_sha(&token) || !looks_like_codex_session_token(&token) {
            continue;
        }
        if !hints.iter().any(|existing| existing == &token) {
            hints.push(token);
            if hints.len() >= 2 {
                break;
            }
        }
    }
    hints
}

fn extract_codex_session_hint(query: &str) -> Option<String> {
    extract_codex_session_hints(query).into_iter().next()
}

fn extract_codex_session_reference_request(
    query_text: &str,
    normalized_query: &str,
) -> Option<CodexSessionReferenceRequest> {
    if starts_with_codex_session_lookup_only_phrase(normalized_query) {
        return None;
    }
    let session_hints = extract_codex_session_hints(normalized_query);
    if session_hints.is_empty() {
        return None;
    }
    let user_request = extract_codex_session_reference_user_request(query_text, &session_hints)?;
    let count = extract_codex_session_reference_count(query_text, &session_hints);
    Some(CodexSessionReferenceRequest {
        session_hints,
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
    session_hints: &[String],
) -> Option<String> {
    extract_codex_session_reference_suffix_user_request(query_text, session_hints)
        .or_else(|| extract_codex_session_reference_prefix_user_request(query_text, session_hints))
}

fn extract_codex_session_reference_suffix_user_request(
    query_text: &str,
    session_hints: &[String],
) -> Option<String> {
    let query_lower = query_text.to_ascii_lowercase();
    let last_hint = session_hints.last()?;
    let hint_lower = last_hint.to_ascii_lowercase();
    let start = query_lower.rfind(&hint_lower)?;
    let after_hint = query_text.get(start + last_hint.len()..)?.trim_start();
    let remainder = strip_codex_session_window_prefix(after_hint)
        .trim_start_matches(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ':' | '-'))
        .trim();
    let remainder = strip_codex_session_followup_prefix(remainder);
    if remainder.is_empty() {
        None
    } else {
        Some(remainder.to_string())
    }
}

fn extract_codex_session_reference_prefix_user_request(
    query_text: &str,
    session_hints: &[String],
) -> Option<String> {
    let query_lower = query_text.to_ascii_lowercase();
    let first_hint = session_hints.first()?;
    let hint_lower = first_hint.to_ascii_lowercase();
    let start = query_lower.find(&hint_lower)?;
    let before_hint = query_text.get(..start)?.trim_end();
    let remainder = strip_codex_session_reference_bridge_suffix(before_hint)?
        .trim_end_matches(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ':' | '-'))
        .trim();
    if remainder.is_empty() {
        None
    } else {
        Some(remainder.to_string())
    }
}

fn strip_codex_session_reference_bridge_suffix(value: &str) -> Option<&str> {
    let trimmed =
        value.trim_end_matches(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ':'));
    for suffix in [
        " with context from codex session",
        " with context from codex sesh",
        " with context from codex chat",
        " with context from session",
        " with context from sesh",
        " with context from chat",
        " with context from thread",
        " using codex session",
        " using codex sesh",
        " using codex chat",
        " using session",
        " using sesh",
        " using chat",
        " using thread",
        " from codex session",
        " from codex sesh",
        " from codex chat",
        " from session",
        " from sesh",
        " from chat",
        " from thread",
        " based on",
        " using",
        " from",
        " via",
    ] {
        if trimmed.len() >= suffix.len()
            && trimmed[trimmed.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        {
            return Some(trimmed[..trimmed.len() - suffix.len()].trim_end());
        }
    }
    None
}

fn strip_codex_session_followup_prefix(value: &str) -> &str {
    let mut remainder = value.trim_start();
    loop {
        let next =
            if remainder.len() >= 14 && remainder[..14].eq_ignore_ascii_case("codex session ") {
                Some(&remainder[14..])
            } else if remainder.len() >= 11 && remainder[..11].eq_ignore_ascii_case("codex sesh ") {
                Some(&remainder[11..])
            } else if remainder.len() >= 12 && remainder[..12].eq_ignore_ascii_case("codex chat ") {
                Some(&remainder[12..])
            } else if remainder.len() >= 6 && remainder[..6].eq_ignore_ascii_case("codex ") {
                Some(&remainder[6..])
            } else if remainder.len() >= 8 && remainder[..8].eq_ignore_ascii_case("session ") {
                Some(&remainder[8..])
            } else if remainder.len() >= 5 && remainder[..5].eq_ignore_ascii_case("sesh ") {
                Some(&remainder[5..])
            } else if remainder.len() >= 5 && remainder[..5].eq_ignore_ascii_case("chat ") {
                Some(&remainder[5..])
            } else if remainder.len() >= 7 && remainder[..7].eq_ignore_ascii_case("thread ") {
                Some(&remainder[7..])
            } else if remainder.len() >= 4 && remainder[..4].eq_ignore_ascii_case("and ") {
                Some(&remainder[4..])
            } else if remainder.len() >= 5 && remainder[..5].eq_ignore_ascii_case("then ") {
                Some(&remainder[5..])
            } else {
                None
            };

        match next {
            Some(rest) => {
                remainder = rest.trim_start_matches(|ch: char| {
                    ch.is_whitespace() || matches!(ch, ',' | ';' | ':' | '-')
                });
            }
            None => return remainder.trim(),
        }
    }
}

fn extract_codex_session_reference_count(query_text: &str, session_hints: &[String]) -> usize {
    let query_lower = query_text.to_ascii_lowercase();
    let Some(last_hint) = session_hints.last() else {
        return 12;
    };
    let hint_lower = last_hint.to_ascii_lowercase();
    let after_hint = query_lower
        .rfind(&hint_lower)
        .and_then(|start| query_text.get(start + last_hint.len()..))
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
    session_hint: &str,
    count: usize,
) -> Result<CodexResolvedReference> {
    let row = read_codex_threads_by_session_hint(session_hint, 1)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No Codex session found for {}", session_hint))?;
    let excerpt = read_last_context(&row.id, Provider::Codex, count, &PathBuf::from(&row.cwd))?;
    Ok(CodexResolvedReference {
        name: "codex-session".to_string(),
        source: "session".to_string(),
        matched: row.id.clone(),
        command: None,
        output: render_codex_session_reference(&row, count, &excerpt),
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

fn parse_codex_versioned_db_filename(file_name: &str, prefix: &str) -> Option<u32> {
    file_name
        .strip_prefix(prefix)?
        .strip_suffix(".sqlite")?
        .parse::<u32>()
        .ok()
}

fn select_codex_state_db_path(sqlite_home: &Path) -> Result<PathBuf> {
    let mut candidates: Vec<(u32, PathBuf)> = fs::read_dir(sqlite_home)
        .with_context(|| format!("failed to read {}", sqlite_home.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let file_name = path.file_name()?.to_str()?;
            let version = parse_codex_versioned_db_filename(file_name, "state_")?;
            Some((version, path))
        })
        .collect();
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    if let Some((_, path)) = candidates.into_iter().next() {
        return Ok(path);
    }

    let legacy_path = sqlite_home.join("state.sqlite");
    if legacy_path.exists() {
        return Ok(legacy_path);
    }

    bail!(
        "no Codex state_<version>.sqlite database found under {}",
        sqlite_home.display()
    )
}

pub(crate) fn codex_state_db_path() -> Result<PathBuf> {
    select_codex_state_db_path(&codex_sqlite_home()?)
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

fn codex_session_completion_markers_dir() -> Result<PathBuf> {
    Ok(config::ensure_global_state_dir()?
        .join("codex")
        .join("session-completions"))
}

fn codex_session_completion_scan_limit() -> usize {
    env::var("FLOW_CODEX_SESSION_COMPLETION_SCAN_LIMIT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.clamp(1, 200))
        .unwrap_or(CODEX_SESSION_COMPLETION_DEFAULT_SCAN_LIMIT)
}

fn codex_session_completion_idle_secs() -> u64 {
    env::var("FLOW_CODEX_SESSION_COMPLETION_IDLE_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| value.clamp(15, 3600))
        .unwrap_or(CODEX_SESSION_COMPLETION_DEFAULT_IDLE_SECS)
}

fn prune_codex_session_completion_markers(now_unix: u64) -> Result<()> {
    let root = codex_session_completion_markers_dir()?;
    if !root.exists() {
        return Ok(());
    }
    let keep_cutoff = now_unix.saturating_sub(60 * 24 * 60 * 60);
    let Ok(entries) = fs::read_dir(&root) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_secs())
            .unwrap_or(now_unix);
        if modified < keep_cutoff {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn claim_codex_session_completion_marker(session_id: &str, assistant_at_unix: u64) -> Result<bool> {
    let root = codex_session_completion_markers_dir()?;
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;
    let key = blake3::hash(format!("{session_id}:{assistant_at_unix}").as_bytes()).to_hex();
    let path = root.join(format!("{key}.done"));
    let mut file = match OpenOptions::new().create_new(true).write(true).open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to create {}", path.display()));
        }
    };
    writeln!(file, "{session_id}:{assistant_at_unix}")
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
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

fn codex_thread_schema_cache() -> &'static Mutex<HashMap<PathBuf, CodexThreadSchemaCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CodexThreadSchemaCacheEntry>>> = OnceLock::new();
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

fn read_codex_thread_schema(conn: &Connection) -> Result<CodexThreadSchema> {
    let mut stmt = conn
        .prepare("pragma table_info(threads)")
        .context("failed to prepare threads schema query")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .context("failed to query threads schema")?;
    let mut names = BTreeSet::new();
    for column in columns {
        names.insert(column?);
    }
    Ok(CodexThreadSchema {
        has_rollout_path: names.contains("rollout_path"),
        has_model: names.contains("model"),
        has_reasoning_effort: names.contains("reasoning_effort"),
    })
}

pub(crate) fn load_codex_thread_schema(db_path: &Path) -> Result<CodexThreadSchema> {
    let stamp = codex_state_db_stamp(db_path)?;
    if let Ok(cache) = codex_thread_schema_cache().lock() {
        if let Some(entry) = cache.get(db_path) {
            if entry.stamp == stamp {
                return Ok(entry.schema.clone());
            }
        }
    }

    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to open {}", db_path.display()))?;
    let schema = read_codex_thread_schema(&conn)?;
    if let Ok(mut cache) = codex_thread_schema_cache().lock() {
        cache.insert(
            db_path.to_path_buf(),
            CodexThreadSchemaCacheEntry {
                stamp,
                schema: schema.clone(),
            },
        );
    }
    Ok(schema)
}

pub(crate) fn codex_recover_select_sql(schema: &CodexThreadSchema) -> String {
    let rollout_expr = if schema.has_rollout_path {
        "rollout_path"
    } else {
        "NULL as rollout_path"
    };
    let model_expr = if schema.has_model {
        "model"
    } else {
        "NULL as model"
    };
    let reasoning_expr = if schema.has_reasoning_effort {
        "reasoning_effort"
    } else {
        "NULL as reasoning_effort"
    };
    format!(
        r#"
select
  id,
  {rollout_expr},
  updated_at,
  cwd,
  title,
  first_user_message,
  git_branch,
  {model_expr},
  {reasoning_expr}
from threads
"#
    )
}

pub(crate) fn map_codex_recover_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CodexRecoverRow> {
    Ok(CodexRecoverRow {
        id: row.get("id")?,
        rollout_path: row.get("rollout_path")?,
        updated_at: row.get("updated_at")?,
        cwd: row.get("cwd")?,
        title: row.get("title")?,
        first_user_message: row.get("first_user_message")?,
        git_branch: row.get("git_branch")?,
        model: row.get("model")?,
        reasoning_effort: row.get("reasoning_effort")?,
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

pub(crate) fn codex_find_now_unix() -> i64 {
    Utc::now().timestamp()
}

pub(crate) fn codex_find_recency_bonus(updated_at: i64, now_unix: i64) -> i64 {
    let age_secs = if updated_at >= now_unix {
        0
    } else {
        now_unix - updated_at
    };

    if age_secs <= CODEX_FIND_RECENT_DAY_SECS {
        16
    } else if age_secs <= (CODEX_FIND_DEFAULT_RECENT_DAYS as i64 * CODEX_FIND_RECENT_DAY_SECS) {
        12
    } else if age_secs <= CODEX_FIND_RECENT_MONTH_SECS {
        4
    } else {
        0
    }
}

pub(crate) fn codex_find_path_affinity(
    cwd: &str,
    target_path: Option<&Path>,
    exact_cwd: bool,
) -> i64 {
    let Some(target_path) = target_path else {
        return 0;
    };

    let cwd_path = Path::new(cwd);
    if cwd_path == target_path {
        return if exact_cwd { 64 } else { 56 };
    }
    if exact_cwd {
        return 0;
    }

    let Ok(relative) = cwd_path.strip_prefix(target_path) else {
        return 0;
    };
    match relative.components().count() {
        0 => 56,
        1 => 24,
        2 => 18,
        3 => 12,
        _ => 6,
    }
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
    let db_path = codex_state_db_path()?;
    let schema = load_codex_thread_schema(&db_path)?;

    let target = target_path.to_string_lossy().to_string();
    let like_target = format!("{}/%", escape_like(&target));
    let fetch_limit = (limit.max(3) * 12).min(120);
    let cache_key = format!("target={target}\nexact={exact_cwd}\nfetch_limit={fetch_limit}");

    let sql_exact = format!(
        r#"
{}
where archived = 0
  and cwd = ?1
order by updated_at desc
limit ?2
"#,
        codex_recover_select_sql(&schema)
    );

    let sql_tree = format!(
        r#"
{}
where archived = 0
  and (cwd = ?1 or cwd like ?2 escape '\')
order by updated_at desc
limit ?3
"#,
        codex_recover_select_sql(&schema)
    );

    let mut rows = with_codex_query_cache(&db_path, "recent", &cache_key, |conn| {
        if exact_cwd {
            let mut stmt = conn
                .prepare(&sql_exact)
                .context("failed to prepare exact recover query")?;
            let iter =
                stmt.query_map(params![target, fetch_limit as i64], map_codex_recover_row)?;
            Ok(iter.collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = conn
                .prepare(&sql_tree)
                .context("failed to prepare subtree recover query")?;
            let iter = stmt.query_map(
                params![target, like_target, fetch_limit as i64],
                map_codex_recover_row,
            )?;
            Ok(iter.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    })?;

    rank_recover_rows(&mut rows, query, None, exact_cwd);
    rows.truncate(limit.max(1));
    Ok(rows)
}

fn read_recent_codex_threads_global_local(limit: usize) -> Result<Vec<CodexRecoverRow>> {
    let db_path = codex_state_db_path()?;
    let schema = load_codex_thread_schema(&db_path)?;
    let fetch_limit = limit.clamp(1, 200);
    let cache_key = format!("limit={fetch_limit}");
    let sql = format!(
        r#"
{}
where archived = 0
order by updated_at desc
limit ?1
"#,
        codex_recover_select_sql(&schema)
    );

    with_codex_query_cache(&db_path, "recent-global", &cache_key, |conn| {
        let mut stmt = conn
            .prepare(&sql)
            .context("failed to prepare global recover query")?;
        let iter = stmt.query_map(params![fetch_limit as i64], map_codex_recover_row)?;
        Ok(iter.collect::<rusqlite::Result<Vec<_>>>()?)
    })
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
    let db_path = codex_state_db_path()?;
    let schema = load_codex_thread_schema(&db_path)?;
    let normalized_hint = session_hint.trim().to_ascii_lowercase();
    if normalized_hint.is_empty() {
        return Ok(vec![]);
    }
    let cache_key = format!("hint={normalized_hint}\nlimit={}", limit.max(1));

    let sql = format!(
        r#"
{}
where archived = 0
  and (lower(id) = ?1 or lower(id) like ?2 escape '\')
order by
  case when lower(id) = ?1 then 0 else 1 end,
  updated_at desc
limit ?3
"#,
        codex_recover_select_sql(&schema)
    );

    let prefix_like = format!("{}%", escape_like(&normalized_hint));
    with_codex_query_cache(&db_path, "session-hint", &cache_key, |conn| {
        let mut stmt = conn
            .prepare(&sql)
            .context("failed to prepare explicit session recover query")?;
        let iter = stmt.query_map(
            params![normalized_hint, prefix_like, limit.max(1) as i64],
            map_codex_recover_row,
        )?;
        Ok(iter.collect::<rusqlite::Result<Vec<_>>>()?)
    })
}

fn search_codex_threads_for_find(
    target_path: Option<&Path>,
    exact_cwd: bool,
    query: &str,
    limit: usize,
    scope: CodexFindScope,
) -> Result<Vec<CodexRecoverRow>> {
    match codexd::query_find(target_path, exact_cwd, query, limit, scope) {
        Ok(rows) if !rows.is_empty() => Ok(rows),
        Ok(_) => {
            debug!("codexd find query returned no rows; falling back to local query");
            search_codex_threads_for_find_local(target_path, exact_cwd, query, limit, scope)
        }
        Err(err) => {
            debug!(error = %err, "codexd find query failed; falling back to local query");
            search_codex_threads_for_find_local(target_path, exact_cwd, query, limit, scope)
        }
    }
}

pub(crate) fn search_codex_threads_for_find_local(
    target_path: Option<&Path>,
    exact_cwd: bool,
    query: &str,
    limit: usize,
    scope: CodexFindScope,
) -> Result<Vec<CodexRecoverRow>> {
    let normalized_query = query.trim().to_lowercase();
    if normalized_query.is_empty() {
        return Ok(vec![]);
    }
    let now_unix = codex_find_now_unix();

    if let Some(session_hint) = extract_codex_session_hint(&normalized_query) {
        let rows = read_codex_threads_by_session_hint_local(&session_hint, limit.max(1))?;
        if !rows.is_empty() {
            return Ok(rows);
        }
    }

    let mut index_hits = Vec::new();
    match codex_session_index::search_codex_sessions(
        target_path,
        exact_cwd,
        &normalized_query,
        limit.max(1),
        scope,
    ) {
        Ok(hits) => index_hits = hits,
        Err(err) => {
            debug!(error = %err, "codex session index query failed; falling back to local SQL search");
        }
    }

    let mut rows = search_codex_threads_for_find_legacy_local(
        target_path,
        exact_cwd,
        &normalized_query,
        limit,
        now_unix,
        scope,
    )?;
    rows = merge_index_find_matches(
        index_hits,
        rows,
        &normalized_query,
        now_unix,
        target_path,
        exact_cwd,
    );
    rows = merge_transcript_find_matches(
        rows,
        target_path,
        exact_cwd,
        &normalized_query,
        limit.max(1),
        now_unix,
        scope,
    )?;
    rows.truncate(limit.max(1));
    Ok(rows)
}

fn search_codex_threads_for_find_legacy_local(
    target_path: Option<&Path>,
    exact_cwd: bool,
    normalized_query: &str,
    limit: usize,
    now_unix: i64,
    scope: CodexFindScope,
) -> Result<Vec<CodexRecoverRow>> {
    let recent_cutoff = scope.recent_cutoff_unix(now_unix);
    let mut rows = search_codex_threads_for_find_legacy_local_with_cutoff(
        target_path,
        exact_cwd,
        normalized_query,
        limit,
        recent_cutoff,
    )?;
    if recent_cutoff.is_some()
        && should_expand_find_scope(
            &rows,
            normalized_query,
            now_unix,
            target_path,
            exact_cwd,
            limit.max(1),
        )
    {
        let expanded_rows = search_codex_threads_for_find_legacy_local_with_cutoff(
            target_path,
            exact_cwd,
            normalized_query,
            limit,
            None,
        )?;
        rows = merge_legacy_find_rows(
            rows,
            expanded_rows,
            normalized_query,
            now_unix,
            target_path,
            exact_cwd,
        );
    }
    Ok(rows)
}

fn search_codex_threads_for_find_legacy_local_with_cutoff(
    target_path: Option<&Path>,
    exact_cwd: bool,
    normalized_query: &str,
    limit: usize,
    recent_cutoff_unix: Option<i64>,
) -> Result<Vec<CodexRecoverRow>> {
    let db_path = codex_state_db_path()?;
    let schema = load_codex_thread_schema(&db_path)?;

    let mut sql = codex_recover_select_sql(&schema);
    sql.push_str("where archived = 0\n");
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

    if let Some(recent_cutoff_unix) = recent_cutoff_unix {
        sql.push_str("  and updated_at >= ?\n");
        params_vec.push(Box::new(recent_cutoff_unix));
    }

    let search_terms = codex_find_search_terms(normalized_query);
    let mut clauses = Vec::new();
    let mut search_columns = vec!["id", "first_user_message", "title", "git_branch", "cwd"];
    if schema.has_model {
        search_columns.push("model");
    }
    if schema.has_reasoning_effort {
        search_columns.push("reasoning_effort");
    }
    for term in search_terms {
        let pattern = format!("%{}%", escape_like(&term));
        for column in &search_columns {
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
        "query={normalized_query}\nexact={exact_cwd}\ntarget={scope_target}\nfetch_limit={fetch_limit}\nrecent_cutoff={}",
        recent_cutoff_unix
            .map(|value| value.to_string())
            .unwrap_or_else(|| "all".to_string())
    );
    let mut rows = with_codex_query_cache(&db_path, "find", &cache_key, |conn| {
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn
            .prepare(&sql)
            .context("failed to prepare Codex find query")?;
        let iter = stmt.query_map(params_refs.as_slice(), map_codex_recover_row)?;
        Ok(iter.collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    rank_recover_rows(&mut rows, Some(normalized_query), target_path, exact_cwd);
    Ok(rows)
}

fn merge_index_find_matches(
    index_hits: Vec<codex_session_index::CodexSessionIndexHit>,
    rows: Vec<CodexRecoverRow>,
    normalized_query: &str,
    now_unix: i64,
    target_path: Option<&Path>,
    exact_cwd: bool,
) -> Vec<CodexRecoverRow> {
    let tokens = tokenize_recover_query(normalized_query);
    let mut merged = BTreeMap::<String, (CodexRecoverRow, i64)>::new();

    for hit in index_hits {
        merged.insert(hit.row.id.clone(), (hit.row, hit.score));
    }

    for row in rows {
        merged.entry(row.id.clone()).or_insert((row, 0));
    }

    let mut values = merged.into_values().collect::<Vec<_>>();
    values.sort_by(|(row_a, index_score_a), (row_b, index_score_b)| {
        let lexical_a = recover_row_score(row_a, normalized_query, &tokens);
        let lexical_b = recover_row_score(row_b, normalized_query, &tokens);
        let total_a = recover_find_rank_score(
            row_a,
            normalized_query,
            &tokens,
            now_unix,
            target_path,
            exact_cwd,
        ) * 20
            + if lexical_a > 0 {
                *index_score_a
            } else {
                *index_score_a / 6
            };
        let total_b = recover_find_rank_score(
            row_b,
            normalized_query,
            &tokens,
            now_unix,
            target_path,
            exact_cwd,
        ) * 20
            + if lexical_b > 0 {
                *index_score_b
            } else {
                *index_score_b / 6
            };
        total_b
            .cmp(&total_a)
            .then_with(|| row_b.updated_at.cmp(&row_a.updated_at))
            .then_with(|| row_a.cwd.cmp(&row_b.cwd))
    });

    values.into_iter().map(|(row, _)| row).collect()
}

fn should_expand_find_scope(
    rows: &[CodexRecoverRow],
    normalized_query: &str,
    now_unix: i64,
    target_path: Option<&Path>,
    exact_cwd: bool,
    limit: usize,
) -> bool {
    let tokens = tokenize_recover_query(normalized_query);
    rows.len() < limit
        || rows
            .first()
            .map(|row| {
                recover_find_rank_score(
                    row,
                    normalized_query,
                    &tokens,
                    now_unix,
                    target_path,
                    exact_cwd,
                )
            })
            .unwrap_or_default()
            < CODEX_FIND_STRONG_MATCH_SCORE
}

fn merge_legacy_find_rows(
    rows: Vec<CodexRecoverRow>,
    expanded_rows: Vec<CodexRecoverRow>,
    normalized_query: &str,
    now_unix: i64,
    target_path: Option<&Path>,
    exact_cwd: bool,
) -> Vec<CodexRecoverRow> {
    let tokens = tokenize_recover_query(normalized_query);
    let mut merged = BTreeMap::<String, CodexRecoverRow>::new();

    for row in rows.into_iter().chain(expanded_rows.into_iter()) {
        merged.entry(row.id.clone()).or_insert(row);
    }

    let mut values = merged.into_values().collect::<Vec<_>>();
    values.sort_by(|row_a, row_b| {
        let total_a = recover_find_rank_score(
            row_a,
            normalized_query,
            &tokens,
            now_unix,
            target_path,
            exact_cwd,
        );
        let total_b = recover_find_rank_score(
            row_b,
            normalized_query,
            &tokens,
            now_unix,
            target_path,
            exact_cwd,
        );
        total_b
            .cmp(&total_a)
            .then_with(|| row_b.updated_at.cmp(&row_a.updated_at))
            .then_with(|| row_a.cwd.cmp(&row_b.cwd))
    });
    values
}

fn merge_transcript_find_matches(
    rows: Vec<CodexRecoverRow>,
    target_path: Option<&Path>,
    exact_cwd: bool,
    normalized_query: &str,
    limit: usize,
    now_unix: i64,
    _scope: CodexFindScope,
) -> Result<Vec<CodexRecoverRow>> {
    let should_scan_transcripts = target_path.is_some() || rows.is_empty();
    if !should_scan_transcripts {
        return Ok(rows);
    }

    let transcript_rows =
        search_codex_transcript_rows(target_path, exact_cwd, normalized_query, limit, now_unix)?;
    if transcript_rows.is_empty() {
        return Ok(rows);
    }

    let tokens = tokenize_recover_query(normalized_query);
    let mut merged = BTreeMap::<String, (CodexRecoverRow, i64)>::new();
    for (row, transcript_score) in transcript_rows {
        merged.insert(row.id.clone(), (row, transcript_score));
    }
    for row in rows {
        merged.entry(row.id.clone()).or_insert((row, 0));
    }

    let mut values = merged.into_values().collect::<Vec<_>>();
    values.sort_by(|(row_a, transcript_score_a), (row_b, transcript_score_b)| {
        let lexical_a = recover_row_score(row_a, normalized_query, &tokens);
        let lexical_b = recover_row_score(row_b, normalized_query, &tokens);
        let total_a = recover_find_rank_score(
            row_a,
            normalized_query,
            &tokens,
            now_unix,
            target_path,
            exact_cwd,
        ) * 20
            + if lexical_a > 0 {
                *transcript_score_a
            } else {
                *transcript_score_a / 6
            };
        let total_b = recover_find_rank_score(
            row_b,
            normalized_query,
            &tokens,
            now_unix,
            target_path,
            exact_cwd,
        ) * 20
            + if lexical_b > 0 {
                *transcript_score_b
            } else {
                *transcript_score_b / 6
            };
        total_b
            .cmp(&total_a)
            .then_with(|| row_b.updated_at.cmp(&row_a.updated_at))
            .then_with(|| row_a.cwd.cmp(&row_b.cwd))
    });
    Ok(values.into_iter().map(|(row, _)| row).collect())
}

fn search_codex_transcript_rows(
    target_path: Option<&Path>,
    exact_cwd: bool,
    normalized_query: &str,
    limit: usize,
    now_unix: i64,
) -> Result<Vec<(CodexRecoverRow, i64)>> {
    let terms = codex_find_search_terms(normalized_query);
    if terms.is_empty() {
        return Ok(vec![]);
    }

    let scan_limit = (limit.max(3) * 4).clamp(8, 16);
    let candidate_rows = match target_path {
        Some(path) => read_recent_codex_threads_local(path, exact_cwd, scan_limit, None)?,
        None => read_recent_codex_threads_global_local(scan_limit)?,
    };

    let mut matches = Vec::new();
    for row in candidate_rows {
        let score = codex_transcript_match_score_for_row(&row, normalized_query, &terms)?;
        if score > 0 {
            let transcript_score = score
                + codex_find_recency_bonus(row.updated_at, now_unix)
                + codex_find_path_affinity(&row.cwd, target_path, exact_cwd);
            matches.push((row, transcript_score));
        }
    }

    matches.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.0.updated_at.cmp(&a.0.updated_at))
            .then_with(|| a.0.cwd.cmp(&b.0.cwd))
    });

    Ok(matches.into_iter().take(limit.max(1)).collect())
}

fn codex_transcript_match_score_for_row(
    row: &CodexRecoverRow,
    normalized_query: &str,
    terms: &[String],
) -> Result<i64> {
    let Some(session_file) = codex_session_file_for_recover_row(row) else {
        return Ok(0);
    };
    codex_transcript_match_score(&session_file, normalized_query, terms)
}

fn codex_session_file_for_recover_row(row: &CodexRecoverRow) -> Option<PathBuf> {
    row.rollout_path
        .as_deref()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .or_else(|| find_codex_session_file(&row.id))
}

pub(crate) fn read_codex_session_search_excerpt(
    row: &CodexRecoverRow,
    max_snippets: usize,
    max_chars: usize,
) -> Result<Option<String>> {
    let Some(session_file) = codex_session_file_for_recover_row(row) else {
        return Ok(None);
    };

    let mut snippets = VecDeque::new();
    for_each_nonempty_jsonl_line(&session_file, |line| {
        let entry: CodexEntry = match crate::json_parse::parse_json_line(line) {
            Ok(value) => value,
            Err(_) => return,
        };
        let Some((role, text)) = extract_codex_message(&entry) else {
            return;
        };
        let clean = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if clean.is_empty() {
            return;
        }

        let role_label = if role.eq_ignore_ascii_case("assistant") {
            "Assistant"
        } else {
            "User"
        };
        let snippet_limit = (max_chars / max_snippets.max(1)).clamp(80, 280);
        let snippet = truncate_search_excerpt_text(&clean, snippet_limit);
        if snippet.is_empty() {
            return;
        }
        if snippets.len() == max_snippets.max(1) {
            snippets.pop_front();
        }
        snippets.push_back(format!("{role_label}: {snippet}"));
    })?;

    if snippets.is_empty() {
        return Ok(None);
    }

    let joined = snippets.into_iter().collect::<Vec<_>>().join("\n");
    Ok(Some(truncate_search_excerpt_text(
        &joined,
        max_chars.max(120),
    )))
}

fn codex_transcript_match_score(
    session_file: &Path,
    normalized_query: &str,
    terms: &[String],
) -> Result<i64> {
    if normalized_query.trim().is_empty() {
        return Ok(0);
    }

    let mut score = 0i64;
    let mut phrase_matched = false;
    let mut matched_terms = BTreeSet::new();

    for_each_nonempty_jsonl_line(session_file, |line| {
        let entry: CodexEntry = match crate::json_parse::parse_json_line(line) {
            Ok(value) => value,
            Err(_) => return,
        };
        let Some((_role, text)) = extract_codex_message(&entry) else {
            return;
        };
        let haystack = text.to_lowercase();

        if !phrase_matched && haystack.contains(normalized_query) {
            score += 900;
            phrase_matched = true;
        }

        for term in terms {
            if term == normalized_query || term.len() <= 2 {
                continue;
            }
            if haystack.contains(term) && matched_terms.insert(term.clone()) {
                score += if term.contains('/') { 90 } else { 70 };
            }
        }
    })?;

    Ok(score)
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

pub(crate) fn tokenize_recover_query(query: &str) -> Vec<String> {
    query
        .split(|ch: char| {
            !ch.is_ascii_alphanumeric() && ch != '/' && ch != '-' && ch != '_' && ch != '#'
        })
        .filter(|part| !part.is_empty())
        .map(|part| part.to_lowercase())
        .filter(|part| part.len() > 1)
        .collect()
}

pub(crate) fn is_structured_find_token(token: &str) -> bool {
    token.contains('/') || token.contains('-') || token.contains('_') || token.contains('#')
}

pub(crate) fn codex_find_search_text(role: &str, value: Option<&str>) -> String {
    let Some(raw) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return String::new();
    };
    normalize_session_message(role, raw)
        .unwrap_or_else(|| raw.to_string())
        .to_lowercase()
}

fn rank_recover_rows(
    rows: &mut Vec<CodexRecoverRow>,
    query: Option<&str>,
    target_path: Option<&Path>,
    exact_cwd: bool,
) {
    let normalized_query = query.map(|q| q.to_lowercase()).unwrap_or_default();
    let tokens = tokenize_recover_query(&normalized_query);
    let now_unix = codex_find_now_unix();

    rows.sort_by(|a, b| {
        let score_a = recover_find_rank_score(
            a,
            &normalized_query,
            &tokens,
            now_unix,
            target_path,
            exact_cwd,
        );
        let score_b = recover_find_rank_score(
            b,
            &normalized_query,
            &tokens,
            now_unix,
            target_path,
            exact_cwd,
        );
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

pub(crate) fn recover_row_score(
    row: &CodexRecoverRow,
    normalized_query: &str,
    tokens: &[String],
) -> i64 {
    if tokens.is_empty() && normalized_query.is_empty() {
        return 0;
    }

    let id = row.id.to_lowercase();
    let cwd = row.cwd.to_lowercase();
    let branch = row.git_branch.clone().unwrap_or_default().to_lowercase();
    let model = row.model.clone().unwrap_or_default().to_lowercase();
    let reasoning_effort = row
        .reasoning_effort
        .clone()
        .unwrap_or_default()
        .to_lowercase();
    let title = codex_find_search_text("user", row.title.as_deref());
    let first = codex_find_search_text("user", row.first_user_message.as_deref());

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
        if model.contains(normalized_query) {
            score += 65;
        }
        if reasoning_effort.contains(normalized_query) {
            score += 30;
        }
        if cwd.contains(normalized_query) {
            score += 60;
        }
    }

    for token in tokens {
        let structured = is_structured_find_token(token);
        if id.starts_with(token) {
            score += if structured { 110 } else { 90 };
        } else if id.contains(token) {
            score += if structured { 72 } else { 60 };
        }
        if first.contains(token) {
            score += if structured { 42 } else { 18 };
        }
        if title.contains(token) {
            score += if structured { 32 } else { 14 };
        }
        if branch.contains(token) {
            score += if structured { 20 } else { 12 };
        }
        if model.contains(token) {
            score += if structured { 18 } else { 12 };
        }
        if reasoning_effort.contains(token) {
            score += if structured { 10 } else { 6 };
        }
        if cwd.contains(token) {
            score += if structured { 12 } else { 8 };
        }
    }

    score
}

fn recover_find_rank_score(
    row: &CodexRecoverRow,
    normalized_query: &str,
    tokens: &[String],
    now_unix: i64,
    target_path: Option<&Path>,
    exact_cwd: bool,
) -> i64 {
    recover_row_score(row, normalized_query, tokens)
        + codex_find_recency_bonus(row.updated_at, now_unix)
        + codex_find_path_affinity(&row.cwd, target_path, exact_cwd)
}

fn build_recover_output(
    target_path: &Path,
    exact_cwd: bool,
    query: Option<String>,
    rows: Vec<CodexRecoverRow>,
) -> CodexRecoverOutput {
    let candidates = rows_to_recover_candidates(rows);

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

fn rows_to_recover_candidates(rows: Vec<CodexRecoverRow>) -> Vec<CodexRecoverCandidate> {
    rows.into_iter()
        .map(|row| CodexRecoverCandidate {
            id: row.id,
            updated_at: format_unix_ts(row.updated_at),
            updated_at_unix: row.updated_at,
            cwd: row.cwd,
            git_branch: row.git_branch.filter(|value| !value.trim().is_empty()),
            model: row.model.filter(|value| !value.trim().is_empty()),
            reasoning_effort: row
                .reasoning_effort
                .filter(|value| !value.trim().is_empty()),
            title: row.title.filter(|value| !value.trim().is_empty()),
            first_user_message: row
                .first_user_message
                .filter(|value| !value.trim().is_empty()),
        })
        .collect()
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

fn truncate_search_excerpt_text(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let clean = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.chars().count() <= max_chars {
        return clean;
    }
    let keep = max_chars.saturating_sub(3);
    let truncated: String = clean.chars().take(keep).collect();
    format!("{truncated}...")
}

fn format_unix_ts(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|value| value.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn codex_model_label(model: Option<&str>, reasoning_effort: Option<&str>) -> Option<String> {
    match (
        model.map(str::trim).filter(|value| !value.is_empty()),
        reasoning_effort
            .map(str::trim)
            .filter(|value| !value.is_empty()),
    ) {
        (Some(model), Some(reasoning_effort)) => Some(format!("{model} [{reasoning_effort}]")),
        (Some(model), None) => Some(model.to_string()),
        (None, Some(reasoning_effort)) => Some(format!("reasoning {reasoning_effort}")),
        (None, None) => None,
    }
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
        if let Some(model) = codex_model_label(
            candidate.model.as_deref(),
            candidate.reasoning_effort.as_deref(),
        ) {
            println!("  model: {}", model);
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
    scope: CodexFindScope,
    provider: Provider,
) -> Result<()> {
    if provider != Provider::Codex {
        bail!("connect is only supported for Codex sessions; use `f codex connect ...`");
    }

    let target_path = resolve_codex_connect_target_path(path)?;
    let query_text = query.join(" ").trim().to_string();
    if should_fast_path_codex_connect(&query_text, exact_cwd, json_output) {
        ensure_provider_tty(Provider::Codex, "connect")?;
        if launch_codex_continue_last_for_target(Some(&target_path))? {
            record_codex_connect_activity(
                "resume latest recent session",
                "latest",
                &target_path,
                &target_path,
                None,
            );
            return Ok(());
        }
    }
    let normalized_query = query_text.to_ascii_lowercase();
    let resolved = if query_text.is_empty() {
        read_recent_codex_threads(&target_path, exact_cwd, 1, None)?
            .into_iter()
            .next()
            .map(|row| (row, "latest recent session".to_string()))
    } else {
        resolve_codex_session_lookup(
            &target_path,
            exact_cwd,
            &query_text,
            &normalized_query,
            scope,
        )?
        .or_else(|| {
            search_codex_threads_for_find(Some(&target_path), exact_cwd, &query_text, 1, scope)
                .ok()?
                .into_iter()
                .next()
                .map(|row| (row, "matched session search query".to_string()))
        })
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
                "model": row.model,
                "reasoningEffort": row.reasoning_effort,
                "reason": reason,
                "targetPath": target_path.display().to_string(),
                "exactCwd": exact_cwd,
                "recentDays": scope.effective_recent_days(),
                "allHistory": scope.all_history,
                "query": if query_text.is_empty() { None::<String> } else { Some(query_text) },
            }))
            .context("failed to encode codex connect JSON")?
        );
        return Ok(());
    }

    ensure_provider_tty(Provider::Codex, "connect")?;
    let connect_summary = if query_text.is_empty() {
        row.first_user_message
            .as_deref()
            .and_then(codex_text::sanitize_codex_query_text)
            .or_else(|| row.title.as_deref().map(str::trim).map(str::to_string))
            .unwrap_or_else(|| "resume latest recent session".to_string())
    } else {
        query_text.clone()
    };
    let launch_path = PathBuf::from(&row.cwd);
    record_codex_connect_activity(
        &connect_summary,
        if query_text.is_empty() {
            "latest"
        } else {
            "query"
        },
        &target_path,
        &launch_path,
        Some(&row.id),
    );

    println!(
        "Resuming session {} from {}...",
        &row.id[..8.min(row.id.len())],
        launch_path.display()
    );
    if launch_session_for_target(
        &row.id,
        Provider::Codex,
        None,
        Some(&launch_path),
        None,
        None,
    )? {
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

pub fn codex_resolve_inspector(
    path: Option<String>,
    query: String,
    exact_cwd: bool,
) -> Result<CodexResolveInspectorResponse> {
    let plan = build_codex_open_plan(path, vec![query], exact_cwd)?;
    let runtime_skills = load_runtime_skills_from_plan(&plan)?;
    let workflow = build_codex_resolve_workflow_explanation(&plan, &runtime_skills);
    Ok(CodexResolveInspectorResponse {
        action: plan.action,
        route: plan.route,
        reason: plan.reason,
        target_path: plan.target_path,
        launch_path: plan.launch_path,
        query: plan.query,
        session_id: plan.session_id,
        prompt: plan.prompt,
        references: plan
            .references
            .into_iter()
            .map(|reference| CodexResolveReferenceSnapshot {
                name: reference.name,
                source: reference.source,
                matched: reference.matched,
                command: reference.command,
                output: reference.output,
            })
            .collect(),
        runtime_state_path: plan.runtime_state_path,
        runtime_skills,
        prompt_context_budget_chars: plan.prompt_context_budget_chars,
        max_resolved_references: plan.max_resolved_references,
        prompt_chars: plan.prompt_chars,
        injected_context_chars: plan.injected_context_chars,
        trace: plan.trace,
        workflow,
    })
}

fn build_codex_resolve_workflow_explanation(
    plan: &CodexOpenPlan,
    runtime_skills: &[CodexResolveRuntimeSkillSnapshot],
) -> Option<CodexResolveWorkflowExplanation> {
    if let Some(reference) = plan
        .references
        .iter()
        .find(|reference| reference.name == "pr-feedback")
    {
        return Some(build_pr_feedback_workflow_explanation(
            plan,
            reference,
            runtime_skills,
        ));
    }

    if let Some(reference) = plan
        .references
        .iter()
        .find(|reference| reference.name == "commit-workflow")
    {
        let repo_root = Path::new(&plan.target_path);
        let kit_gate = detect_commit_workflow_kit_gate(repo_root);
        return Some(CodexResolveWorkflowExplanation {
            id: "commit-workflow".to_string(),
            title: "Commit workflow".to_string(),
            summary: "Flow recognized a high-confidence commit request and turned it into a guarded commit workflow instead of passing plain `commit` text through to Codex.".to_string(),
            trigger: "High-confidence commit language like `commit`, `commit and push`, or `review and commit`.".to_string(),
            generated_by: "flow backend route metadata".to_string(),
            packet: CodexResolveWorkflowPacket {
                kind: "commit_workflow".to_string(),
                compact_summary: "Compact commit packet seeded with repo status and diff stats instead of a full pasted diff.".to_string(),
                default_view: "Start with git status and compact diff stats. Open the full diff or long repo instructions only when the compact view does not explain the risk.".to_string(),
                expansion_rules: vec![
                    "Read the compact repo snapshot first.".to_string(),
                    "Inspect the exact local diff and adjacent call sites only after the compact view tells you where to look.".to_string(),
                    "Use repo AGENTS/review instructions as binding constraints, but do not expand them into the prompt unless the change depends on them.".to_string(),
                ],
                trace: plan.trace.clone(),
                validation_plan: {
                    let mut plan = vec![
                        CodexResolveWorkflowValidation {
                            label: "Diff inspection".to_string(),
                            tier: "targeted".to_string(),
                            detail: "Inspect the actual local diff and adjacent call sites before deciding on the final commit shape.".to_string(),
                            command: None,
                        },
                        CodexResolveWorkflowValidation {
                            label: "Smallest safety check".to_string(),
                            tier: "targeted".to_string(),
                            detail: "Run the smallest test, lint, or manual check that can falsify the change before committing.".to_string(),
                            command: None,
                        },
                    ];
                    if let Some(command) = kit_gate {
                        plan.push(CodexResolveWorkflowValidation {
                            label: "Deterministic repo gate".to_string(),
                            tier: "targeted".to_string(),
                            detail: "Run the repo's deterministic gate before the final commit when one is available.".to_string(),
                            command: Some(command),
                        });
                    }
                    if let Some(command) = reference.command.clone() {
                        plan.push(CodexResolveWorkflowValidation {
                            label: "Final guarded commit lane".to_string(),
                            tier: "operator".to_string(),
                            detail: "Use the slower Flow-assisted commit path for the final review and commit synthesis instead of a fast blind commit.".to_string(),
                            command: Some(command),
                        });
                    }
                    plan
                },
            },
            commands: reference
                .command
                .as_deref()
                .map(|command| {
                    vec![CodexResolveWorkflowCommand {
                        label: "Preferred command".to_string(),
                        command: command.to_string(),
                    }]
                })
                .unwrap_or_default(),
            artifacts: Vec::new(),
            steps: vec![
                CodexResolveWorkflowStep {
                    title: "Inspect the real repo state".to_string(),
                    detail: "Flow snapshots the repo status and diff context before Codex starts so the commit flow is grounded in actual local changes.".to_string(),
                },
                CodexResolveWorkflowStep {
                    title: "Inject the commit contract".to_string(),
                    detail: "The prompt includes a commit contract focused on correctness, regression risk, performance, robustness, and repo `AGENTS.md` compliance.".to_string(),
                },
                CodexResolveWorkflowStep {
                    title: "Bias toward deterministic gates".to_string(),
                    detail: "Repo-specific gates such as Kit lint/review are surfaced in the contract so the commit lane does not depend only on model judgment.".to_string(),
                },
            ],
            notes: vec![
                format!("Route: {}", plan.route),
                format!("Reason: {}", plan.reason),
            ],
        });
    }

    if let Some(reference) = plan
        .references
        .iter()
        .find(|reference| reference.name == "sync-workflow")
    {
        return Some(CodexResolveWorkflowExplanation {
            id: "sync-workflow".to_string(),
            title: "Sync workflow".to_string(),
            summary: "Flow recognized guarded sync language and routed it into the repo's safe sync workflow instead of leaving Codex to improvise branch sync behavior.".to_string(),
            trigger: "High-confidence sync language like `sync branch` in a supported repo/workspace.".to_string(),
            generated_by: "flow backend route metadata".to_string(),
            packet: CodexResolveWorkflowPacket {
                kind: "sync_workflow".to_string(),
                compact_summary: "Compact sync packet carrying the guarded repo sync command and repo workflow contract.".to_string(),
                default_view: "Start with the repo-specific sync contract. Only expand broader branch history or repo instructions if the guarded sync command reports a blocker.".to_string(),
                expansion_rules: vec![
                    "Use the repo's guarded sync path first.".to_string(),
                    "Inspect additional branch history only when sync reports a blocker.".to_string(),
                    "Keep sync explanations branch-aware and compact instead of replaying full Git/JJ history.".to_string(),
                ],
                trace: plan.trace.clone(),
                validation_plan: vec![
                    CodexResolveWorkflowValidation {
                        label: "Guarded sync command".to_string(),
                        tier: "targeted".to_string(),
                        detail: "Use the repo sync contract rather than improvising Git/JJ steps.".to_string(),
                        command: reference.command.clone(),
                    },
                    CodexResolveWorkflowValidation {
                        label: "Post-sync status check".to_string(),
                        tier: "targeted".to_string(),
                        detail: "Confirm what changed, whether the branch is now synced, and whether any blocker remains.".to_string(),
                        command: None,
                    },
                ],
            },
            commands: reference
                .command
                .as_deref()
                .map(|command| {
                    vec![CodexResolveWorkflowCommand {
                        label: "Preferred command".to_string(),
                        command: command.to_string(),
                    }]
                })
                .unwrap_or_default(),
            artifacts: Vec::new(),
            steps: vec![
                CodexResolveWorkflowStep {
                    title: "Map plain sync language to the repo workflow".to_string(),
                    detail: "Flow chooses the repo-specific sync command so branch movement stays consistent with the local workflow instead of defaulting to raw git operations.".to_string(),
                },
                CodexResolveWorkflowStep {
                    title: "Keep the main prompt compact".to_string(),
                    detail: "Only the sync contract and relevant repo instructions are injected, which avoids bloating normal coding context.".to_string(),
                },
            ],
            notes: vec![
                format!("Route: {}", plan.route),
                format!("Reason: {}", plan.reason),
            ],
        });
    }

    None
}

fn build_pr_feedback_workflow_explanation(
    plan: &CodexOpenPlan,
    reference: &CodexResolvedReference,
    runtime_skills: &[CodexResolveRuntimeSkillSnapshot],
) -> CodexResolveWorkflowExplanation {
    let fields = parse_reference_fields(&reference.output);
    let mut commands = Vec::new();
    if let Some(command) = reference.command.as_deref() {
        commands.push(CodexResolveWorkflowCommand {
            label: "Primary command".to_string(),
            command: command.to_string(),
        });
    }
    if let Some(command) = fields.get("cursor reopen") {
        commands.push(CodexResolveWorkflowCommand {
            label: "Cursor reopen".to_string(),
            command: command.clone(),
        });
    }

    let mut artifacts = Vec::new();
    push_workflow_artifact(&mut artifacts, "Workspace", fields.get("workspace"), "path");
    push_workflow_artifact(
        &mut artifacts,
        "Snapshot markdown",
        fields.get("snapshot markdown"),
        "path",
    );
    push_workflow_artifact(
        &mut artifacts,
        "Snapshot json",
        fields.get("snapshot json"),
        "path",
    );
    push_workflow_artifact(
        &mut artifacts,
        "Review plan",
        fields.get("review plan"),
        "path",
    );
    push_workflow_artifact(
        &mut artifacts,
        "Review rules",
        fields.get("review rules"),
        "path",
    );
    push_workflow_artifact(
        &mut artifacts,
        "Kit system prompt",
        fields.get("kit system prompt"),
        "path",
    );
    push_workflow_artifact(&mut artifacts, "Trace ID", fields.get("trace id"), "text");
    push_workflow_artifact(&mut artifacts, "PR URL", fields.get("url"), "url");
    push_workflow_artifact(
        &mut artifacts,
        "PR feedback",
        fields.get("pr feedback"),
        "text",
    );

    let skill_note = runtime_skills
        .iter()
        .find(|skill| {
            skill.name == "github"
                || skill.original_name.as_deref() == Some("github")
                || skill.name.contains("github")
        })
        .map(|skill| {
            let mut note = format!(
                "Runtime skill: {}",
                skill
                    .original_name
                    .as_deref()
                    .unwrap_or(skill.name.as_str())
            );
            if let Some(reason) = skill.match_reason.as_deref() {
                note.push_str(" — ");
                note.push_str(reason);
            }
            note
        });

    let mut notes = vec![
        format!("Route: {}", plan.route),
        format!("Reason: {}", plan.reason),
        "This explanation is generated by Flow backend code, so myflow stays aligned with the current route behavior instead of duplicating docs in the UI.".to_string(),
    ];
    if let Some(note) = skill_note {
        notes.push(note);
    }

    CodexResolveWorkflowExplanation {
        id: "pr-feedback".to_string(),
        title: "GitHub PR review workflow".to_string(),
        summary: "Flow recognized the prompt as PR review intent, ran the PR feedback pipeline, generated a reusable review packet, injected compact review context into Codex, and loaded the GitHub runtime skill.".to_string(),
        trigger: "GitHub pull-request URL plus review language like `check`, `comments`, `review`, or `for comments`.".to_string(),
        generated_by: "flow backend route metadata".to_string(),
        packet: CodexResolveWorkflowPacket {
            kind: "pr_feedback".to_string(),
            compact_summary: "Compact PR review packet with artifact paths, top review items, and a review-plan handoff instead of the full GitHub page.".to_string(),
            default_view: "Start with the compact PR packet and the generated review plan. Expand snapshot markdown/json only when a review item needs more original context.".to_string(),
            expansion_rules: vec![
                "Read the compact packet first.".to_string(),
                "Use the generated review plan as the working ledger for item-by-item resolution.".to_string(),
                "Open snapshot markdown/json only when the packet or review plan is insufficient.".to_string(),
            ],
            trace: plan.trace.clone(),
            validation_plan: {
                let mut plan = vec![CodexResolveWorkflowValidation {
                    label: "Per-item product validation".to_string(),
                    tier: "targeted".to_string(),
                    detail: "For each review item, run the smallest relevant test, lint, or manual repro in the product repo before marking it resolved.".to_string(),
                    command: None,
                }];
                if let Some(review_plan) = fields.get("review plan") {
                    plan.push(CodexResolveWorkflowValidation {
                        label: "Use the review ledger".to_string(),
                        tier: "operator".to_string(),
                        detail: "Keep the generated review plan up to date as the item-by-item source of truth instead of starting each item from an empty chat.".to_string(),
                        command: Some(review_plan.clone()),
                    });
                }
                if let Some(command) = fields.get("cursor reopen") {
                    plan.push(CodexResolveWorkflowValidation {
                        label: "Reopen the full review surface".to_string(),
                        tier: "operator".to_string(),
                        detail: "Reopen the workspace and review artifacts together when you need the full human + Cursor review loop again.".to_string(),
                        command: Some(command.clone()),
                    });
                }
                plan
            },
        },
        commands,
        artifacts,
        steps: vec![
            CodexResolveWorkflowStep {
                title: "Route the URL as builtin `pr-feedback`".to_string(),
                detail: "Flow treats the prompt as review intent instead of a generic web URL, so the route is deterministic and review-specific.".to_string(),
            },
            CodexResolveWorkflowStep {
                title: "Run the PR feedback pipeline".to_string(),
                detail: "Flow effectively runs `f pr feedback <url>` and uses `gh` to fetch the PR title, reviews, review comments, and issue comments.".to_string(),
            },
            CodexResolveWorkflowStep {
                title: "Write the review packet".to_string(),
                detail: "Flow writes the markdown/json feedback snapshot, the human review plan, the review rules artifact, and the Kit system prompt.".to_string(),
            },
            CodexResolveWorkflowStep {
                title: "Inject compact review context".to_string(),
                detail: "Codex receives a compact PR-review block with the generated artifact paths and top feedback items instead of the full GitHub page.".to_string(),
            },
            CodexResolveWorkflowStep {
                title: "Load the GitHub runtime skill".to_string(),
                detail: "The runtime state activates the GitHub skill alongside the builtin route so follow-up GitHub CLI work has the right context.".to_string(),
            },
            CodexResolveWorkflowStep {
                title: "Drive the item-by-item review loop".to_string(),
                detail: "The generated review packet is what `forge review`, the Flow Review panel, and the Kit follow-up prompts use for the actual resolution workflow.".to_string(),
            },
        ],
        notes,
    }
}

fn parse_reference_fields(output: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('[') {
            continue;
        }
        let Some((label, value)) = trimmed.split_once(": ") else {
            continue;
        };
        if matches!(label, "Summary" | "Top feedback items" | "Plan excerpt") {
            continue;
        }
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        fields.insert(label.to_ascii_lowercase(), value.to_string());
    }
    fields
}

fn push_workflow_artifact(
    artifacts: &mut Vec<CodexResolveWorkflowArtifact>,
    label: &str,
    value: Option<&String>,
    kind: &str,
) {
    if let Some(value) = value {
        artifacts.push(CodexResolveWorkflowArtifact {
            label: label.to_string(),
            value: value.clone(),
            kind: kind.to_string(),
        });
    }
}

fn load_runtime_skills_from_plan(
    plan: &CodexOpenPlan,
) -> Result<Vec<CodexResolveRuntimeSkillSnapshot>> {
    let Some(path) = plan.runtime_state_path.as_deref() else {
        return Ok(Vec::new());
    };
    let raw = fs::read(path).with_context(|| format!("failed to read runtime state {}", path))?;
    let state: codex_runtime::CodexRuntimeState = serde_json::from_slice(&raw)
        .with_context(|| format!("failed to decode runtime state {}", path))?;
    Ok(state
        .skills
        .into_iter()
        .map(|skill| CodexResolveRuntimeSkillSnapshot {
            name: skill.name,
            kind: skill.kind,
            path: skill.path,
            trigger: skill.trigger,
            source: skill.source,
            original_name: skill.original_name,
            estimated_chars: skill.estimated_chars,
            match_reason: skill.match_reason,
        })
        .collect())
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

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexDoctorSnapshot {
    target: String,
    codex_bin: String,
    codexd: String,
    codexd_socket: String,
    run_agent_bridge: String,
    run_agent_router: String,
    run_agent_count: usize,
    run_agent_bridge_error: Option<String>,
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
    pub project_ai: ai_project_manifest::AiProjectManifest,
    pub skills: codex_runtime::CodexSkillsDashboardSnapshot,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexEvalRouteSnapshot {
    pub route: String,
    pub count: usize,
    pub share: f64,
    pub avg_context_chars: f64,
    pub avg_reference_count: f64,
    pub runtime_activation_rate: f64,
    pub last_recorded_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexEvalSkillSnapshot {
    pub name: String,
    pub score: f64,
    pub sample_size: usize,
    pub outcome_samples: usize,
    pub pass_rate: f64,
    pub normalized_gain: f64,
    pub avg_context_chars: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexEvalOpportunity {
    pub severity: String,
    pub title: String,
    pub detail: String,
    pub next_step: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexEvalCommand {
    pub label: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexEvalSnapshot {
    pub generated_at_unix: u64,
    pub target_path: String,
    pub sample_limit: usize,
    pub recent_events: usize,
    pub recent_outcomes: usize,
    pub summary: String,
    pub quality: CodexEvalQualitySnapshot,
    pub doctor: CodexDoctorSnapshot,
    pub top_routes: Vec<CodexEvalRouteSnapshot>,
    pub top_skills: Vec<CodexEvalSkillSnapshot>,
    pub opportunities: Vec<CodexEvalOpportunity>,
    pub commands: Vec<CodexEvalCommand>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexEvalQualitySnapshot {
    pub status: String,
    pub summary: String,
    pub failure_modes: Vec<String>,
    pub grounded: bool,
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
    let run_agent_bridge = collect_run_agent_bridge_status();
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
    if run_agent_bridge.status != "ready" {
        let detail = run_agent_bridge
            .error
            .clone()
            .unwrap_or_else(|| "unknown error".to_string());
        warnings.push(format!(
            "run-agent bridge is {}; Flow cannot launch ~/run agents directly ({detail})",
            run_agent_bridge.status
        ));
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
        run_agent_bridge: run_agent_bridge.status,
        run_agent_router: run_agent_bridge.router_path,
        run_agent_count: run_agent_bridge.agent_count,
        run_agent_bridge_error: run_agent_bridge.error,
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
        project_ai: ai_project_manifest::load_for_target(target_path, false)?,
        skills: codex_runtime::dashboard_snapshot(target_path, &codex_cfg, recent_limit)?,
    })
}

pub fn codex_project_ai_snapshot(
    target_path: &Path,
    refresh: bool,
) -> Result<ai_project_manifest::AiProjectManifest> {
    ai_project_manifest::load_for_target(target_path, refresh)
}

pub fn codex_project_ai_recent(
    limit: usize,
) -> Result<Vec<ai_project_manifest::AiProjectManifest>> {
    ai_project_manifest::recent(limit)
}

pub fn codex_skill_source_sync(
    target_path: &Path,
    selected_skills: &[String],
    force: bool,
) -> Result<usize> {
    let codex_cfg = load_codex_config_for_path(target_path);
    codex_runtime::sync_external_skills(target_path, &codex_cfg, selected_skills, force)
}

fn print_codex_doctor(snapshot: &CodexDoctorSnapshot) {
    println!("# codex doctor");
    println!("target: {}", snapshot.target);
    println!("codex_bin: {}", snapshot.codex_bin);
    println!("codexd: {}", snapshot.codexd);
    println!("codexd_socket: {}", snapshot.codexd_socket);
    println!("run_agent_bridge: {}", snapshot.run_agent_bridge);
    println!("run_agent_router: {}", snapshot.run_agent_router);
    println!("run_agent_count: {}", snapshot.run_agent_count);
    if let Some(error) = &snapshot.run_agent_bridge_error {
        println!("run_agent_bridge_error: {}", error);
    }
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

fn codexd_learning_refresh_interval_secs() -> u64 {
    std::env::var("FLOW_CODEXD_LEARNING_REFRESH_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| value.clamp(60, 3600))
        .unwrap_or(900)
}

fn codexd_learning_refresh_state() -> &'static Mutex<u64> {
    static STATE: OnceLock<Mutex<u64>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(0))
}

pub(crate) fn maybe_run_codex_learning_refresh() -> Result<usize> {
    let interval_secs = codexd_learning_refresh_interval_secs();
    let now = unix_now_secs();
    {
        let mut guard = codexd_learning_refresh_state()
            .lock()
            .expect("codexd learning refresh mutex poisoned");
        if now.saturating_sub(*guard) < interval_secs {
            return Ok(0);
        }
        *guard = now;
    }

    let _ = codex_memory::sync_from_skill_eval_logs(400);
    let targets = codex_skill_eval::recent_targets(400, 10, 168)?;
    let mut refreshed = 0usize;
    for target in targets {
        if !target.exists() {
            continue;
        }
        codex_skill_eval::rebuild_scorecard(&target, 200)?;
        refreshed += 1;
    }
    Ok(refreshed)
}

fn codex_eval_commands(target_path: &Path) -> Vec<CodexEvalCommand> {
    let target = target_path.display().to_string();
    vec![
        CodexEvalCommand {
            label: "Doctor".to_string(),
            command: format!("f codex doctor --path {}", target),
        },
        CodexEvalCommand {
            label: "Autonomous readiness".to_string(),
            command: format!("f codex doctor --path {} --assert-autonomous", target),
        },
        CodexEvalCommand {
            label: "Skill scorecard".to_string(),
            command: format!("f codex skill-eval show --path {}", target),
        },
        CodexEvalCommand {
            label: "Recent memory".to_string(),
            command: format!("f codex memory recent --path {} --limit 12", target),
        },
        CodexEvalCommand {
            label: "Daemon status".to_string(),
            command: "f codex daemon status".to_string(),
        },
    ]
}

fn codex_eval_failure_modes(doctor: &CodexDoctorSnapshot) -> Vec<String> {
    let mut failure_modes = Vec::new();
    if doctor.runtime_transport != "enabled" {
        failure_modes.push("wrapper transport disabled".to_string());
    }
    if doctor.runtime_skills != "enabled" {
        failure_modes.push(format!("runtime skills {}", doctor.runtime_skills));
    }
    if doctor.memory_state != "ready" {
        failure_modes.push("codex memory unavailable".to_string());
    }
    failure_modes
}

fn build_codex_eval_quality(
    doctor: &CodexDoctorSnapshot,
    recent_events: usize,
    recent_outcomes: usize,
) -> CodexEvalQualitySnapshot {
    let failure_modes = codex_eval_failure_modes(doctor);
    let status = if failure_modes.is_empty() {
        "valid"
    } else {
        "erroneous"
    };
    let grounded = recent_outcomes > 0 && doctor.learning_ready;
    let summary = if status == "erroneous" {
        format!(
            "Current target health is erroneous for workflow measurement because Flow is not fully controlling Codex here: {}.",
            failure_modes.join(", ")
        )
    } else if grounded {
        "Current target health is valid and grounded outcome samples are available.".to_string()
    } else if recent_events == 0 {
        "Current target health is valid, but there are no recorded Flow-routed Codex events here yet.".to_string()
    } else {
        "Current target health is valid, but measurements are still warming up because there are no grounded outcome samples yet.".to_string()
    };

    CodexEvalQualitySnapshot {
        status: status.to_string(),
        summary,
        failure_modes,
        grounded,
    }
}

fn build_codex_eval_summary(
    doctor: &CodexDoctorSnapshot,
    events: usize,
    outcomes: usize,
    top_route: Option<&CodexEvalRouteSnapshot>,
    top_skill: Option<&CodexEvalSkillSnapshot>,
) -> String {
    if events == 0 {
        return "No Flow-routed Codex events recorded yet for this target.".to_string();
    }
    if !doctor.runtime_ready {
        return format!(
            "Flow is recording usage, but the Codex wrapper/runtime path is not fully active yet. Recent launches: {}.",
            events
        );
    }
    if outcomes == 0 {
        return format!(
            "Flow is recording {} recent Codex launches here, but there are no grounded outcome samples yet, so learning is still affinity-only.",
            events
        );
    }

    let route = top_route
        .map(|value| value.route.as_str())
        .unwrap_or("unknown");
    let skill = top_skill.map(|value| value.name.as_str()).unwrap_or("none");
    format!(
        "Runtime is ready and grounded learning is active. Recent launches: {}, grounded outcomes: {}, top route: {}, top skill: {}.",
        events, outcomes, route, skill
    )
}

fn build_codex_eval_opportunities(
    doctor: &CodexDoctorSnapshot,
    recent_events: usize,
    recent_outcomes: usize,
    routes: &[CodexEvalRouteSnapshot],
    skills: &[CodexEvalSkillSnapshot],
) -> Vec<CodexEvalOpportunity> {
    let mut opportunities = Vec::new();

    if doctor.runtime_transport != "enabled" || doctor.runtime_skills != "enabled" {
        opportunities.push(CodexEvalOpportunity {
            severity: "high".to_string(),
            title: "Wrapper/runtime path is not fully active".to_string(),
            detail: "Flow cannot reliably improve Codex usage until prompts enter through the Flow wrapper and runtime skills are active.".to_string(),
            next_step: "Run `f codex enable-global --full`, then start Codex through `j`, `L`, or `f codex open ...`.".to_string(),
        });
    }

    if doctor.codexd != "running" {
        opportunities.push(CodexEvalOpportunity {
            severity: "medium".to_string(),
            title: "codexd is not running".to_string(),
            detail: "Recent-session hydration and background completion reconciliation stay cold when the Flow Codex daemon is stopped.".to_string(),
            next_step: "Run `f codex daemon start` to keep session recovery and eval maintenance warm.".to_string(),
        });
    }

    if recent_events > 0 && recent_outcomes == 0 {
        opportunities.push(CodexEvalOpportunity {
            severity: "high".to_string(),
            title: "No grounded outcome samples for this target yet".to_string(),
            detail: "Flow is learning from route history here, but it does not yet have target-scoped success/failure outcomes to tell whether runtime skills are actually helping.".to_string(),
            next_step: "Exercise workflows that emit outcomes through Flow for this repo/path, then rerun `f codex eval --path ...` or `f codex skill-eval show --path ...`.".to_string(),
        });
    } else if recent_outcomes > 0 && doctor.skill_scorecard_entries == 0 {
        opportunities.push(CodexEvalOpportunity {
            severity: "medium".to_string(),
            title: "Scorecard has not been built yet".to_string(),
            detail: "Outcome data exists, but there is no repo-scoped scorecard summarizing which runtime skills are helping.".to_string(),
            next_step: "Run `f codex skill-eval run --path ...` once, or let codexd refresh it in the background.".to_string(),
        });
    }

    if let Some(skill) = skills.first()
        && skill.outcome_samples == 0
    {
        opportunities.push(CodexEvalOpportunity {
            severity: "medium".to_string(),
            title: format!("Top skill `{}` is still affinity-only", skill.name),
            detail: "This skill is triggering often enough to score highly, but Flow has not seen grounded success outcomes for it yet.".to_string(),
            next_step: "Add or reuse a deterministic success marker for the workflow that uses this skill, so outcomes get logged.".to_string(),
        });
    }

    if let Some(skill) = skills.iter().find(|skill| {
        skill.sample_size >= 3 && skill.outcome_samples >= 2 && skill.pass_rate < 0.55
    }) {
        opportunities.push(CodexEvalOpportunity {
            severity: "medium".to_string(),
            title: format!("Skill `{}` is underperforming", skill.name),
            detail: format!(
                "It has {} grounded outcome sample(s) with pass rate {:.2}.",
                skill.outcome_samples, skill.pass_rate
            ),
            next_step: "Inspect the skill trigger, gotchas, and injected context. Trim or sharpen it before adding more automation.".to_string(),
        });
    }

    if let Some(route) = routes
        .iter()
        .find(|route| route.count >= 3 && route.avg_context_chars > 1800.0)
    {
        opportunities.push(CodexEvalOpportunity {
            severity: "low".to_string(),
            title: format!("Route `{}` is context-heavy", route.route),
            detail: format!(
                "Average injected context is {:.0} chars across {} recent launch(es).",
                route.avg_context_chars, route.count
            ),
            next_step: "Trim the workflow packet or sharpen the reference unrolling so the route stays compact.".to_string(),
        });
    }

    if let Some(route) = routes.first()
        && route.route == "new-plain"
        && route.share >= 0.7
        && doctor.external_skill_candidates > 0
    {
        opportunities.push(CodexEvalOpportunity {
            severity: "low".to_string(),
            title: "Most launches are still plain prompts".to_string(),
            detail: "That is not necessarily bad, but it suggests the repo has more opportunity for explicit workflow routes or sharper runtime skill triggers.".to_string(),
            next_step: "Inspect common prompts in the recent events and decide whether one should become a first-class workflow or skill trigger.".to_string(),
        });
    }

    if !doctor.schedule_ready && doctor.codexd != "running" {
        opportunities.push(CodexEvalOpportunity {
            severity: "low".to_string(),
            title: "No background refresh is active".to_string(),
            detail: "Scorecards only refresh when you run commands manually if neither launchd nor codexd is keeping the learning data warm.".to_string(),
            next_step: "Install the launchd refresher with `f codex enable-global --full` or keep `codexd` running.".to_string(),
        });
    }

    if opportunities.is_empty() {
        opportunities.push(CodexEvalOpportunity {
            severity: "info".to_string(),
            title: "No immediate weaknesses detected".to_string(),
            detail: "Flow runtime, grounding, and recent skill usage look healthy for this repo/path.".to_string(),
            next_step: "Keep using `f codex eval --path ...` after workflow changes to catch regressions early.".to_string(),
        });
    }

    opportunities
}

pub fn codex_eval_snapshot(target_path: &Path, limit: usize) -> Result<CodexEvalSnapshot> {
    let _ = reconcile_pending_codex_quick_launches(limit.max(64));
    let doctor = collect_codex_doctor_snapshot(target_path)?;
    let events = codex_skill_eval::load_events(Some(target_path), limit)?;
    let outcomes = codex_skill_eval::load_outcomes(Some(target_path), limit)?;
    let latest_event_at = events
        .first()
        .map(|event| event.recorded_at_unix)
        .unwrap_or(0);
    let scorecard = match codex_skill_eval::load_scorecard(target_path)? {
        Some(scorecard) if scorecard.generated_at_unix >= latest_event_at => scorecard,
        _ => codex_skill_eval::rebuild_scorecard(target_path, limit.max(200))?,
    };

    #[derive(Default)]
    struct RouteAggregate {
        count: usize,
        total_context_chars: usize,
        total_reference_count: usize,
        runtime_activations: usize,
        last_recorded_at_unix: u64,
    }

    let mut route_aggregates: BTreeMap<String, RouteAggregate> = BTreeMap::new();
    for event in &events {
        let entry = route_aggregates.entry(event.route.clone()).or_default();
        entry.count += 1;
        entry.total_context_chars += event.injected_context_chars;
        entry.total_reference_count += event.reference_count;
        if !event.runtime_skills.is_empty() {
            entry.runtime_activations += 1;
        }
        entry.last_recorded_at_unix = entry.last_recorded_at_unix.max(event.recorded_at_unix);
    }

    let event_count = events.len().max(1) as f64;
    let mut top_routes = route_aggregates
        .into_iter()
        .map(|(route, agg)| CodexEvalRouteSnapshot {
            route,
            count: agg.count,
            share: agg.count as f64 / event_count,
            avg_context_chars: agg.total_context_chars as f64 / agg.count as f64,
            avg_reference_count: agg.total_reference_count as f64 / agg.count as f64,
            runtime_activation_rate: agg.runtime_activations as f64 / agg.count as f64,
            last_recorded_at_unix: agg.last_recorded_at_unix,
        })
        .collect::<Vec<_>>();
    top_routes.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| b.last_recorded_at_unix.cmp(&a.last_recorded_at_unix))
    });
    top_routes.truncate(6);

    let mut top_skills = scorecard
        .skills
        .iter()
        .map(|skill| CodexEvalSkillSnapshot {
            name: skill.name.clone(),
            score: skill.score,
            sample_size: skill.sample_size,
            outcome_samples: skill.outcome_samples,
            pass_rate: skill.pass_rate,
            normalized_gain: skill.normalized_gain,
            avg_context_chars: skill.avg_context_chars,
        })
        .collect::<Vec<_>>();
    top_skills.truncate(6);

    let summary = build_codex_eval_summary(
        &doctor,
        events.len(),
        outcomes.len(),
        top_routes.first(),
        top_skills.first(),
    );
    let quality = build_codex_eval_quality(&doctor, events.len(), outcomes.len());
    let opportunities = build_codex_eval_opportunities(
        &doctor,
        events.len(),
        outcomes.len(),
        &top_routes,
        &top_skills,
    );

    Ok(CodexEvalSnapshot {
        generated_at_unix: unix_now_secs(),
        target_path: target_path.display().to_string(),
        sample_limit: limit,
        recent_events: events.len(),
        recent_outcomes: outcomes.len(),
        summary,
        quality,
        doctor,
        top_routes,
        top_skills,
        opportunities,
        commands: codex_eval_commands(target_path),
    })
}

fn print_codex_eval(snapshot: &CodexEvalSnapshot) {
    println!("# codex eval");
    println!("target: {}", snapshot.target_path);
    println!("summary: {}", snapshot.summary);
    println!("quality: {}", snapshot.quality.status);
    println!("quality_summary: {}", snapshot.quality.summary);
    println!("sample_limit: {}", snapshot.sample_limit);
    println!("recent_events: {}", snapshot.recent_events);
    println!("recent_outcomes: {}", snapshot.recent_outcomes);
    println!("runtime_ready: {}", snapshot.doctor.runtime_ready);
    println!("learning_ready: {}", snapshot.doctor.learning_ready);
    println!("codexd: {}", snapshot.doctor.codexd);
    if !snapshot.quality.failure_modes.is_empty() {
        println!("failure_modes:");
        for mode in &snapshot.quality.failure_modes {
            println!("- {}", mode);
        }
    }
    if !snapshot.top_routes.is_empty() {
        println!("routes:");
        for route in &snapshot.top_routes {
            println!(
                "- {} | count {} | share {:.0}% | ctx {:.0} chars | refs {:.1} | runtime {:.0}%",
                route.route,
                route.count,
                route.share * 100.0,
                route.avg_context_chars,
                route.avg_reference_count,
                route.runtime_activation_rate * 100.0
            );
        }
    }
    if !snapshot.top_skills.is_empty() {
        println!("skills:");
        for skill in &snapshot.top_skills {
            println!(
                "- {} | score {:.2} | samples {} | outcomes {} | pass {:.2} | gain {:.3} | ctx {:.0} chars",
                skill.name,
                skill.score,
                skill.sample_size,
                skill.outcome_samples,
                skill.pass_rate,
                skill.normalized_gain,
                skill.avg_context_chars
            );
        }
    }
    if !snapshot.opportunities.is_empty() {
        println!("opportunities:");
        for item in &snapshot.opportunities {
            println!("- [{}] {} — {}", item.severity, item.title, item.detail);
            println!("  next: {}", item.next_step);
        }
    }
    if !snapshot.commands.is_empty() {
        println!("commands:");
        for command in &snapshot.commands {
            println!("- {}: {}", command.label, command.command);
        }
    }
}

fn codex_eval(path: Option<String>, limit: usize, json: bool, provider: Provider) -> Result<()> {
    if provider != Provider::Codex {
        bail!("eval is only supported for Codex sessions; use `f codex eval`");
    }

    let target_path = resolve_session_target_path(path.as_deref())?;
    let snapshot = codex_eval_snapshot(&target_path, limit.clamp(20, 1000))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&snapshot).context("failed to encode codex eval JSON")?
        );
    } else {
        print_codex_eval(&snapshot);
    }
    Ok(())
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

fn file_modified_unix(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|value| value.as_secs())
}

fn read_codex_session_completion_snapshot(
    session_file: &Path,
) -> Result<Option<CodexSessionCompletionSnapshot>> {
    let file_modified_unix = file_modified_unix(session_file).unwrap_or(0);
    let mut snapshot = CodexSessionCompletionSnapshot {
        last_role: None,
        last_user_message: None,
        last_user_at_unix: None,
        last_assistant_message: None,
        last_assistant_at_unix: None,
        file_modified_unix,
    };

    for_each_nonempty_jsonl_line(session_file, |line| {
        let entry: CodexEntry = match crate::json_parse::parse_json_line(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        let Some((role, text)) = extract_codex_message(&entry) else {
            return;
        };
        let Some(ts) =
            extract_codex_timestamp(&entry).and_then(|value| parse_rfc3339_to_unix(&value))
        else {
            return;
        };

        snapshot.last_role = Some(role.clone());
        match role.as_str() {
            "user" => {
                snapshot.last_user_message = Some(text);
                snapshot.last_user_at_unix = Some(ts);
            }
            "assistant" => {
                snapshot.last_assistant_message = Some(text);
                snapshot.last_assistant_at_unix = Some(ts);
            }
            _ => {}
        }
    })?;

    if snapshot.last_role.is_none() {
        return Ok(None);
    }
    Ok(Some(snapshot))
}

fn assistant_completion_summary(text: &str) -> Option<String> {
    let cleaned = codex_text::sanitize_codex_memory_rollout_text(text)?;
    let first_line = cleaned
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let summary = first_line
        .trim_start_matches(|ch: char| matches!(ch, '-' | '*' | ' '))
        .trim();
    if summary.is_empty() {
        return None;
    }

    let lower = summary.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "done"
            | "done."
            | "completed"
            | "completed."
            | "implemented"
            | "implemented."
            | "fixed"
            | "fixed."
            | "it's in."
            | "it’s in."
            | "all set."
    ) {
        return None;
    }

    Some(summary.to_string())
}

fn select_codex_session_completion_summary(
    row: &CodexRecoverRow,
    snapshot: &CodexSessionCompletionSnapshot,
) -> String {
    snapshot
        .last_assistant_message
        .as_deref()
        .and_then(assistant_completion_summary)
        .or_else(|| {
            snapshot
                .last_user_message
                .as_deref()
                .and_then(codex_text::sanitize_codex_query_text)
        })
        .or_else(|| {
            row.first_user_message
                .as_deref()
                .and_then(codex_text::sanitize_codex_query_text)
        })
        .or_else(|| row.title.as_deref().map(str::trim).map(str::to_string))
        .unwrap_or_else(|| "completed session turn".to_string())
}

fn build_codex_session_completion_event(
    row: &CodexRecoverRow,
    snapshot: &CodexSessionCompletionSnapshot,
) -> activity_log::ActivityEvent {
    let mut event = activity_log::ActivityEvent::done(
        "codex.done",
        truncate_recover_text(&select_codex_session_completion_summary(row, snapshot)),
    );
    event.target_path = Some(row.cwd.clone());
    event.launch_path = Some(row.cwd.clone());
    event.session_id = Some(row.id.clone());
    event.source = Some("codex-session-completion".to_string());
    event.dedupe_key = snapshot
        .last_assistant_at_unix
        .map(|value| format!("codex:done:{}:{value}", row.id));
    event
}

fn read_codex_turn_patch_changes(
    session_file: &Path,
    since_unix: u64,
    until_unix: u64,
    session_cwd: &str,
) -> Result<Vec<CodexTurnPatchChange>> {
    let mut changes: Vec<CodexTurnPatchChange> = Vec::new();
    for_each_nonempty_jsonl_line(session_file, |line| {
        let entry: CodexEntry = match crate::json_parse::parse_json_line(line) {
            Ok(value) => value,
            Err(_) => return,
        };
        let Some(ts) =
            extract_codex_timestamp(&entry).and_then(|value| parse_rfc3339_to_unix(&value))
        else {
            return;
        };
        if ts < since_unix || ts > until_unix {
            return;
        }

        let Some(payload) = entry.payload.as_ref() else {
            return;
        };
        if entry.entry_type.as_deref() != Some("response_item") {
            return;
        }
        if payload.get("type").and_then(|value| value.as_str()) != Some("custom_tool_call") {
            return;
        }
        if payload.get("status").and_then(|value| value.as_str()) != Some("completed") {
            return;
        }
        if payload.get("name").and_then(|value| value.as_str()) != Some("apply_patch") {
            return;
        }
        let Some(input) = payload.get("input").and_then(|value| value.as_str()) else {
            return;
        };

        for change in parse_apply_patch_changes(input, session_cwd) {
            if let Some(existing) = changes.iter_mut().find(|item| item.path == change.path) {
                if !existing.patch.is_empty() && !change.patch.is_empty() {
                    existing.patch.push('\n');
                }
                existing.patch.push_str(&change.patch);
                if existing.action != change.action {
                    existing.action = "update".to_string();
                }
            } else {
                changes.push(change);
            }
        }
    })?;
    Ok(changes)
}

fn parse_apply_patch_changes(input: &str, session_cwd: &str) -> Vec<CodexTurnPatchChange> {
    let mut changes = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_action = String::new();
    let mut current_patch = String::new();

    let flush_current = |changes: &mut Vec<CodexTurnPatchChange>,
                         current_path: &mut Option<String>,
                         current_action: &mut String,
                         current_patch: &mut String| {
        let Some(path) = current_path.take() else {
            return;
        };
        changes.push(CodexTurnPatchChange {
            path,
            action: std::mem::take(current_action),
            patch: current_patch.trim().to_string(),
        });
        current_patch.clear();
    };

    for line in input.lines() {
        let header = if let Some(path) = line.strip_prefix("*** Update File: ") {
            Some(("update", path))
        } else if let Some(path) = line.strip_prefix("*** Add File: ") {
            Some(("add", path))
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            Some(("delete", path))
        } else {
            None
        };

        if let Some((action, path)) = header {
            flush_current(
                &mut changes,
                &mut current_path,
                &mut current_action,
                &mut current_patch,
            );
            current_action = action.to_string();
            current_path = Some(resolve_patch_path(path, session_cwd));
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Move to: ") {
            current_path = Some(resolve_patch_path(path, session_cwd));
            continue;
        }

        if current_path.is_some() {
            current_patch.push_str(line);
            current_patch.push('\n');
        }
    }

    flush_current(
        &mut changes,
        &mut current_path,
        &mut current_action,
        &mut current_patch,
    );
    changes
}

fn resolve_patch_path(path: &str, session_cwd: &str) -> String {
    let raw = Path::new(path);
    if raw.is_absolute() {
        return raw.display().to_string();
    }
    Path::new(session_cwd).join(raw).display().to_string()
}

fn fish_fn_path() -> String {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("config")
        .join("fish")
        .join("fn.fish")
        .display()
        .to_string()
}

fn is_fish_fn_path(path: &str) -> bool {
    path.ends_with("/config/fish/fn.fish") || path == fish_fn_path()
}

fn summarize_fish_fn_change(text: &str) -> Option<String> {
    let normalized = text.to_ascii_lowercase();
    let mut remaps = Vec::new();
    if normalized.contains("j is now the fresh codex entrypoint")
        || normalized.contains("j runs f codex open")
        || normalized.contains("__flow_codex open --path")
    {
        remaps.push("j->codex.open");
    }
    if normalized.contains("k is now the current-folder codex continue entrypoint")
        || normalized.contains("k uses f codex connect")
        || normalized.contains("__flow_codex connect --path")
    {
        remaps.push("k->codex.connect");
    }
    if normalized.contains("l is now kit")
        || normalized.contains("kit --continue --no-exit")
        || normalized.contains("exec \"$kit_bin\" --continue")
    {
        remaps.push("l->kit");
    }
    if normalized.contains("l now delegates to j")
        || text.contains("function L") && text.contains("j $argv")
    {
        remaps.push("L->j");
    }

    if remaps.is_empty() {
        if normalized.contains("fn.fish") {
            return Some("updated fn.fish".to_string());
        }
        return None;
    }

    let keep_fallbacks = normalized.contains("old k moved to cl")
        || normalized.contains("function cl")
        || normalized.contains("function cf")
        || text.contains("function cF");
    let mut summary = format!("remap {}", remaps.join(", "));
    if keep_fallbacks {
        summary.push_str("; keep cl/cf/cF fallbacks");
    }
    Some(summary)
}

fn build_fish_fn_changed_event(
    row: &CodexRecoverRow,
    snapshot: &CodexSessionCompletionSnapshot,
    summary: String,
) -> activity_log::ActivityEvent {
    let mut event = activity_log::ActivityEvent::changed("fish.fn", summary);
    event.target_path = Some(fish_fn_path());
    event.session_id = Some(row.id.clone());
    event.source = Some("codex-session-change".to_string());
    event.dedupe_key = snapshot
        .last_assistant_at_unix
        .map(|value| format!("codex:changed:{}:{value}:fish.fn", row.id));
    event
}

fn changed_file_label(path: &str) -> String {
    let path_ref = Path::new(path);
    if is_fish_fn_path(path) {
        return "fn.fish".to_string();
    }
    path_ref
        .file_name()
        .and_then(|value| value.to_str())
        .map(|value| value.to_string())
        .unwrap_or_else(|| path.to_string())
}

fn summarize_generic_changed_files(changes: &[CodexTurnPatchChange]) -> String {
    let labels = changes
        .iter()
        .map(|change| changed_file_label(&change.path))
        .collect::<Vec<_>>();
    match labels.len() {
        0 => "updated files".to_string(),
        1 => format!("updated {}", labels[0]),
        2 => format!("updated {}, {}", labels[0], labels[1]),
        _ => format!(
            "updated {}, {} + {} more",
            labels[0],
            labels[1],
            labels.len() - 2
        ),
    }
}

fn build_codex_session_changed_events(
    row: &CodexRecoverRow,
    snapshot: &CodexSessionCompletionSnapshot,
    session_file: &Path,
) -> Result<Vec<activity_log::ActivityEvent>> {
    let mut events = Vec::new();
    let Some(last_assistant_at_unix) = snapshot.last_assistant_at_unix else {
        return Ok(events);
    };

    let patch_changes = snapshot
        .last_user_at_unix
        .map(|last_user_at_unix| {
            read_codex_turn_patch_changes(
                session_file,
                last_user_at_unix,
                last_assistant_at_unix,
                &row.cwd,
            )
        })
        .transpose()?
        .unwrap_or_default();

    let fish_summary = patch_changes
        .iter()
        .find(|change| is_fish_fn_path(&change.path))
        .and_then(|change| summarize_fish_fn_change(&change.patch))
        .or_else(|| {
            snapshot
                .last_assistant_message
                .as_deref()
                .and_then(summarize_fish_fn_change)
        })
        .or_else(|| {
            snapshot
                .last_user_message
                .as_deref()
                .and_then(summarize_fish_fn_change)
        });

    let mut remaining_changes = Vec::new();
    for change in patch_changes {
        if is_fish_fn_path(&change.path) {
            continue;
        }
        remaining_changes.push(change);
    }

    if let Some(summary) = fish_summary {
        events.push(build_fish_fn_changed_event(row, snapshot, summary));
    }

    if !remaining_changes.is_empty() {
        let mut event = activity_log::ActivityEvent::changed(
            "files.changed",
            summarize_generic_changed_files(&remaining_changes),
        );
        event.target_path = Some(row.cwd.clone());
        event.launch_path = Some(row.cwd.clone());
        event.session_id = Some(row.id.clone());
        event.source = Some("codex-session-change".to_string());
        event.dedupe_key = Some(format!(
            "codex:changed:{}:{}:aggregate",
            row.id, last_assistant_at_unix
        ));
        events.push(event);
    }

    Ok(events)
}

fn build_codex_session_doc_input(
    row: &CodexRecoverRow,
    snapshot: &CodexSessionCompletionSnapshot,
    session_file: &Path,
) -> Result<Option<codex_session_docs::CompletedSessionDocInput>> {
    let Some(completed_at_unix) = snapshot.last_assistant_at_unix else {
        return Ok(None);
    };

    let patch_changes = snapshot
        .last_user_at_unix
        .map(|last_user_at_unix| {
            read_codex_turn_patch_changes(
                session_file,
                last_user_at_unix,
                completed_at_unix,
                &row.cwd,
            )
        })
        .transpose()?
        .unwrap_or_default()
        .into_iter()
        .map(|change| codex_session_docs::SessionDocPatchChange {
            path: change.path,
            action: change.action,
            patch: change.patch,
        })
        .collect();

    Ok(Some(codex_session_docs::CompletedSessionDocInput {
        session_id: row.id.clone(),
        session_file: session_file.to_path_buf(),
        target_path: row.cwd.clone(),
        launch_path: Some(row.cwd.clone()),
        first_user_prompt: row.first_user_message.clone(),
        completion_summary: select_codex_session_completion_summary(row, snapshot),
        completed_at_unix,
        patch_changes,
    }))
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
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            workflow_kind: None,
            service_name: None,
        };
        let _ = codex_skill_eval::log_event(&event);
        let _ = log_codex_quick_launch_hydration(&hydration);
        let mut activity_event =
            activity_log::ActivityEvent::done("codex.quick-launch", hydration.query.clone());
        activity_event.route = Some(format!("{}-hydrated", launch.mode));
        activity_event.target_path = Some(hydration.target_path.clone());
        activity_event.launch_path = Some(hydration.target_path.clone());
        activity_event.session_id = Some(hydration.session_id.clone());
        activity_event.source = Some("codex-quick-launch".to_string());
        activity_event.dedupe_key = Some(format!("codex:quick-launch:{}", launch.launch_id));
        let _ = activity_log::append_daily_event(activity_event);
        reconciled += 1;
    }

    Ok(reconciled)
}

pub(crate) fn reconcile_codex_session_completions(limit: usize) -> Result<usize> {
    if limit == 0 {
        return Ok(0);
    }

    let rows = read_recent_codex_threads_global_local(limit)?;
    if rows.is_empty() {
        return Ok(0);
    }

    let now = unix_now_secs();
    let idle_secs = codex_session_completion_idle_secs();
    let _ = prune_codex_session_completion_markers(now);
    let mut reconciled = 0usize;

    for row in rows {
        let Some(session_file) = find_codex_session_file(&row.id) else {
            continue;
        };
        let Some(snapshot) = read_codex_session_completion_snapshot(&session_file)? else {
            continue;
        };
        if snapshot.last_role.as_deref() != Some("assistant") {
            continue;
        }
        let Some(last_assistant_at_unix) = snapshot.last_assistant_at_unix else {
            continue;
        };
        let idle_anchor = snapshot.file_modified_unix.max(last_assistant_at_unix);
        if now.saturating_sub(idle_anchor) < idle_secs {
            continue;
        }
        if !claim_codex_session_completion_marker(&row.id, last_assistant_at_unix)? {
            continue;
        }

        let _ =
            activity_log::append_daily_event(build_codex_session_completion_event(&row, &snapshot));
        for event in build_codex_session_changed_events(&row, &snapshot, &session_file)? {
            let _ = activity_log::append_daily_event(event);
        }
        if let Some(doc_input) = build_codex_session_doc_input(&row, &snapshot, &session_file)? {
            if let Err(err) = codex_session_docs::document_completed_session(&doc_input) {
                eprintln!("WARN codex session docs failed for {}: {err:#}", row.id);
            }
        }
        reconciled += 1;
    }

    Ok(reconciled)
}

pub(crate) fn run_codex_background_maintenance() -> Result<(usize, usize)> {
    let hydrated = reconcile_pending_codex_quick_launches(48)?;
    let completed = reconcile_codex_session_completions(codex_session_completion_scan_limit())?;
    Ok((hydrated, completed))
}

pub(crate) fn maybe_run_codex_telemetry_export(limit: usize) -> Result<usize> {
    codex_telemetry::maybe_flush(limit)
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
    let _ = run_codex_background_maintenance();
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

fn codex_telemetry_command(action: Option<CodexTelemetryAction>, provider: Provider) -> Result<()> {
    if provider != Provider::Codex {
        bail!("telemetry is only supported for Codex sessions; use `f codex telemetry ...`");
    }

    match action.unwrap_or(CodexTelemetryAction::Status { json: false }) {
        CodexTelemetryAction::Status { json } => {
            let status = codex_telemetry::status()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&status)
                        .context("failed to encode codex telemetry status JSON")?
                );
            } else {
                println!("# codex telemetry");
                println!("enabled: {}", status.enabled);
                println!("configured_targets: {}", status.configured_targets);
                println!("service_name: {}", status.service_name);
                println!("scope_name: {}", status.scope_name);
                println!("state_path: {}", status.state_path);
                println!("events_path: {}", status.events_path);
                println!("outcomes_path: {}", status.outcomes_path);
                println!("events_offset: {}", status.events_offset);
                println!("outcomes_offset: {}", status.outcomes_offset);
                println!("events_exported: {}", status.events_exported);
                println!("outcomes_exported: {}", status.outcomes_exported);
                if let Some(last) = status.last_exported_at_unix {
                    println!("last_exported_at_unix: {}", last);
                }
            }
            Ok(())
        }
        CodexTelemetryAction::Flush { limit, json } => {
            let summary = codex_telemetry::flush(limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&summary)
                        .context("failed to encode codex telemetry flush JSON")?
                );
            } else {
                println!("# codex telemetry flush");
                println!("enabled: {}", summary.enabled);
                println!("configured_targets: {}", summary.configured_targets);
                println!("events_seen: {}", summary.events_seen);
                println!("outcomes_seen: {}", summary.outcomes_seen);
                println!("events_exported: {}", summary.events_exported);
                println!("outcomes_exported: {}", summary.outcomes_exported);
                println!("state_path: {}", summary.state_path);
                if let Some(last) = summary.last_exported_at_unix {
                    println!("last_exported_at_unix: {}", last);
                }
            }
            Ok(())
        }
    }
}

fn codex_trace_command(action: Option<CodexTraceAction>, provider: Provider) -> Result<()> {
    if provider != Provider::Codex {
        bail!("trace is only supported for Codex sessions; use `f codex trace ...`");
    }

    match action.unwrap_or(CodexTraceAction::CurrentSession {
        flush: true,
        json: false,
    }) {
        CodexTraceAction::Status { json } => {
            let status = codex_telemetry::trace_status()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&status)
                        .context("failed to encode codex trace status JSON")?
                );
            } else {
                println!("# codex trace");
                println!("enabled: {}", status.enabled);
                println!("endpoint: {}", status.endpoint);
                println!("token_source: {}", status.token_source);
                println!("tools_list_ok: {}", status.tools_list_ok);
                println!("tools_count: {}", status.tools_count);
                println!("read_probe_ok: {}", status.read_probe_ok);
                if let Some(error) = status.read_probe_error.as_deref() {
                    println!("read_probe_error: {}", error);
                }
            }
            Ok(())
        }
        CodexTraceAction::CurrentSession { flush, json } => {
            let current = codex_telemetry::inspect_current_session_trace(flush)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&current)
                        .context("failed to encode current codex trace JSON")?
                );
            } else {
                println!("# codex current-session trace");
                println!("trace_id: {}", current.trace_id);
                if let Some(span_id) = current.span_id.as_deref() {
                    println!("span_id: {}", span_id);
                }
                if let Some(parent_span_id) = current.parent_span_id.as_deref() {
                    println!("parent_span_id: {}", parent_span_id);
                }
                if let Some(workflow_kind) = current.workflow_kind.as_deref() {
                    println!("workflow_kind: {}", workflow_kind);
                }
                if let Some(service_name) = current.service_name.as_deref() {
                    println!("service_name: {}", service_name);
                }
                println!("flushed: {}", current.flushed);
                println!("endpoint: {}", current.endpoint);
                println!("token_source: {}", current.token_source);
                if let Some(error) = current.read_error.as_deref() {
                    println!("read_error: {}", error);
                }
                if let Some(result) = current.result.as_ref() {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(result)
                            .context("failed to encode current codex trace result")?
                    );
                }
            }
            Ok(())
        }
        CodexTraceAction::Inspect {
            trace_id,
            flush,
            json,
        } => {
            let inspected = codex_telemetry::inspect_trace(&trace_id, flush)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&inspected)
                        .context("failed to encode codex trace inspect JSON")?
                );
            } else {
                println!("# codex trace inspect");
                println!("trace_id: {}", inspected.trace_id);
                println!("flushed: {}", inspected.flushed);
                println!("endpoint: {}", inspected.endpoint);
                println!("token_source: {}", inspected.token_source);
                if let Some(error) = inspected.read_error.as_deref() {
                    println!("read_error: {}", error);
                }
                if let Some(result) = inspected.result.as_ref() {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(result)
                            .context("failed to encode codex trace inspect result")?
                    );
                }
            }
            Ok(())
        }
    }
}

fn codex_project_ai_command(
    action: Option<CodexProjectAiAction>,
    provider: Provider,
) -> Result<()> {
    if provider != Provider::Codex {
        bail!("project-ai is only supported for Codex sessions; use `f codex project-ai ...`");
    }

    match action.unwrap_or(CodexProjectAiAction::Show {
        path: None,
        refresh: false,
        json: false,
    }) {
        CodexProjectAiAction::Show {
            path,
            refresh,
            json,
        } => {
            let target_path = resolve_session_target_path(path.as_deref())?;
            let manifest = if codexd::is_running() {
                codexd::query_project_ai_manifest(&target_path, refresh)
                    .or_else(|_| ai_project_manifest::load_for_target(&target_path, refresh))?
            } else {
                ai_project_manifest::load_for_target(&target_path, refresh)?
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&manifest)
                        .context("failed to encode codex project-ai JSON")?
                );
            } else {
                println!("# codex project-ai");
                println!("repo_root: {}", manifest.repo_root);
                println!("generated_at: {}", manifest.generated_at);
                println!("has_ai_dir: {}", manifest.has_ai_dir);
                println!("skills_count: {}", manifest.skills_count);
                println!("docs_count: {}", manifest.docs_count);
                println!("reviews_count: {}", manifest.reviews_count);
                println!("tasks_count: {}", manifest.tasks_count);
                println!("todos_count: {}", manifest.todos_count);
                println!("open_todos_count: {}", manifest.open_todos_count);
                println!("query_count: {}", manifest.query_count);
                if let Some(last) = manifest.last_requested_at_unix {
                    println!("last_requested_at_unix: {}", last);
                }
                if let Some(packet) = manifest.latest_review_packet.as_ref() {
                    println!("latest_review_markdown: {}", packet.markdown_path);
                    if let Some(json_path) = packet.json_path.as_deref() {
                        println!("latest_review_json: {}", json_path);
                    }
                    if let Some(repo_slug) = packet.repo_slug.as_deref() {
                        println!("latest_review_repo: {}", repo_slug);
                    }
                    if let Some(pr_number) = packet.pr_number {
                        println!("latest_review_pr: {}", pr_number);
                    }
                }
                if let Some(context_doc) = manifest.latest_context_doc.as_deref() {
                    println!("latest_context_doc: {}", context_doc);
                }
                if !manifest.latest_skill_names.is_empty() {
                    println!("skills:");
                    for skill in &manifest.latest_skill_names {
                        println!("- {}", skill);
                    }
                }
                if !manifest.latest_task_paths.is_empty() {
                    println!("recent_tasks:");
                    for task in &manifest.latest_task_paths {
                        println!("- {}", task);
                    }
                }
                if !manifest.ignored_local_buckets_present.is_empty() {
                    println!("ignored_local_buckets_present:");
                    for bucket in &manifest.ignored_local_buckets_present {
                        println!("- {}", bucket);
                    }
                }
            }
            Ok(())
        }
        CodexProjectAiAction::Recent { limit, json } => {
            let manifests = if codexd::is_running() {
                codexd::query_recent_project_ai(limit)
                    .or_else(|_| ai_project_manifest::recent(limit))?
            } else {
                ai_project_manifest::recent(limit)?
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&manifests)
                        .context("failed to encode codex project-ai recent JSON")?
                );
            } else if manifests.is_empty() {
                println!("No project-ai manifests have been queried yet.");
            } else {
                println!("# codex project-ai recent");
                for manifest in manifests {
                    println!(
                        "- {} | queries {} | last {} | skills {} | reviews {} | tasks {}",
                        manifest.repo_root,
                        manifest.query_count,
                        manifest.last_requested_at_unix.unwrap_or(0),
                        manifest.skills_count,
                        manifest.reviews_count,
                        manifest.tasks_count
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

fn codex_agent_command(action: Option<CodexAgentAction>, provider: Provider) -> Result<()> {
    if provider != Provider::Codex {
        bail!("agent is only supported for Codex sessions; use `f codex agent ...`");
    }

    match action.unwrap_or(CodexAgentAction::List { json: false }) {
        CodexAgentAction::List { json } => {
            let agents = run_agent_router_list()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&agents)
                        .context("failed to encode codex agent list JSON")?
                );
            } else {
                for agent in agents {
                    println!("{agent}");
                }
            }
            Ok(())
        }
        CodexAgentAction::Show { agent_id } => {
            let output = run_agent_router_show(&agent_id)?;
            print!("{output}");
            if !output.ends_with('\n') {
                println!();
            }
            Ok(())
        }
        CodexAgentAction::Run {
            path,
            new_thread,
            json,
            agent_id,
            query,
        } => {
            let target_path = resolve_session_target_path(path.as_deref())?;
            let query_text = query.join(" ").trim().to_string();
            if query_text.is_empty() {
                bail!("agent run requires a non-empty query");
            }
            let repo_root = detect_git_root(&target_path).unwrap_or_else(|| target_path.clone());
            let completed =
                run_codex_agent_bridge(&agent_id, &target_path, new_thread, &query_text)?;
            record_run_agent_bridge_activity(&agent_id, &target_path, &repo_root, &completed);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&completed)
                        .context("failed to encode codex agent run JSON")?
                );
            } else {
                print_run_agent_completed_event(&completed);
            }
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
            trace: None,
        }));
    };

    let normalized_query = query_text.to_ascii_lowercase();

    if let Some(request) = extract_codex_session_reference_request(&query_text, &normalized_query) {
        let mut references = Vec::new();
        for session_hint in &request.session_hints {
            let reference = resolve_builtin_codex_session_reference(session_hint, request.count)?;
            if !references
                .iter()
                .any(|existing: &CodexResolvedReference| existing.matched == reference.matched)
            {
                references.push(reference);
            }
        }
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
        let route = if request.session_hints.len() > 1 {
            "multi-session-reference-new"
        } else {
            "session-reference-new"
        };
        let reason = if request.session_hints.len() > 1 {
            format!(
                "start a new session with {} resolved Codex session contexts",
                request.session_hints.len()
            )
        } else {
            "start a new session with resolved Codex session context".to_string()
        };
        return Ok(finalize_codex_open_plan(CodexOpenPlan {
            action: "new".to_string(),
            route: route.to_string(),
            reason,
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
            trace: None,
        }));
    }

    if let Some(plan) = build_codex_commit_workflow_plan(
        &target_path,
        &query_text,
        &normalized_query,
        runtime_skills_enabled,
        auto_resolve_references,
        max_resolved_references,
        default_prompt_budget,
        &codex_cfg,
    )? {
        return Ok(plan);
    }

    if let Some(plan) = build_codex_sync_workflow_plan(
        &target_path,
        &query_text,
        &normalized_query,
        max_resolved_references,
        default_prompt_budget,
    )? {
        return Ok(plan);
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

    if let Some((session, reason)) = resolve_codex_session_lookup(
        &target_path,
        exact_cwd,
        &query_text,
        &normalized_query,
        CodexFindScope::default(),
    )? {
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
            trace: None,
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
        trace: None,
    }))
}

fn build_codex_commit_workflow_plan(
    target_path: &Path,
    query_text: &str,
    normalized_query: &str,
    runtime_skills_enabled: bool,
    auto_resolve_references: bool,
    max_resolved_references: usize,
    default_prompt_budget: usize,
    codex_cfg: &config::CodexConfig,
) -> Result<Option<CodexOpenPlan>> {
    if !looks_like_commit_workflow_query(normalized_query) {
        return Ok(None);
    }

    let Some(repo_root) = detect_git_root(target_path) else {
        return Ok(None);
    };

    let mut references = vec![resolve_builtin_commit_workflow_reference(&repo_root)?];
    if auto_resolve_references {
        for reference in
            resolve_codex_references(&repo_root, query_text, &codex_cfg.reference_resolvers)?
        {
            if !references
                .iter()
                .any(|existing| existing.matched == reference.matched)
            {
                references.push(reference);
            }
        }
    }

    let runtime = codex_runtime::prepare_runtime_activation(
        &repo_root,
        query_text,
        runtime_skills_enabled,
        codex_cfg,
    )?;
    let prompt_budget =
        effective_prompt_context_budget_chars(codex_cfg, has_session_reference(&references))
            .max(2200);
    let prompt = build_codex_prompt_with_runtime(
        query_text,
        &references,
        runtime.as_ref(),
        max_resolved_references,
        prompt_budget,
    );

    Ok(Some(finalize_codex_open_plan(CodexOpenPlan {
        action: "new".to_string(),
        route: "commit-workflow-new".to_string(),
        reason: "start a new session with enforced deep-review commit workflow".to_string(),
        target_path: repo_root.display().to_string(),
        launch_path: repo_root.display().to_string(),
        query: Some(query_text.to_string()),
        session_id: None,
        prompt,
        references,
        runtime_state_path: runtime
            .as_ref()
            .map(|value| value.state_path.display().to_string()),
        runtime_skills: runtime_skill_names(runtime.as_ref()),
        prompt_context_budget_chars: prompt_budget.max(default_prompt_budget),
        max_resolved_references,
        prompt_chars: 0,
        injected_context_chars: 0,
        trace: None,
    })))
}

fn build_codex_sync_workflow_plan(
    target_path: &Path,
    query_text: &str,
    normalized_query: &str,
    max_resolved_references: usize,
    default_prompt_budget: usize,
) -> Result<Option<CodexOpenPlan>> {
    if !looks_like_sync_workflow_query(normalized_query) {
        return Ok(None);
    }

    let Some(repo_root) = detect_git_root(target_path) else {
        return Ok(None);
    };
    let Some(command) = detect_sync_workflow_command(&repo_root) else {
        return Ok(None);
    };

    let references = vec![resolve_builtin_sync_workflow_reference(
        &repo_root, &command,
    )?];
    let prompt_budget = default_prompt_budget.max(1600);
    let prompt = build_codex_prompt(
        query_text,
        &references,
        max_resolved_references,
        prompt_budget,
    );

    Ok(Some(finalize_codex_open_plan(CodexOpenPlan {
        action: "new".to_string(),
        route: "sync-workflow-new".to_string(),
        reason: "start a new session with enforced guarded sync workflow".to_string(),
        target_path: repo_root.display().to_string(),
        launch_path: repo_root.display().to_string(),
        query: Some(query_text.to_string()),
        session_id: None,
        prompt,
        references,
        runtime_state_path: None,
        runtime_skills: Vec::new(),
        prompt_context_budget_chars: prompt_budget,
        max_resolved_references,
        prompt_chars: 0,
        injected_context_chars: 0,
        trace: None,
    })))
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
                plan.trace.as_ref(),
            )? {
                Ok(())
            } else {
                bail!("failed to resume codex session {}", session_id);
            }
        }
        "new" | "recover-new" => {
            maybe_open_cursor_for_pr_feedback_check(plan);
            new_session_for_target(
                Provider::Codex,
                plan.prompt.as_deref(),
                Some(&launch_path),
                plan.runtime_state_path.as_deref(),
                plan.trace.as_ref(),
            )
        }
        other => bail!("unsupported codex open action: {}", other),
    }
}

fn maybe_open_cursor_for_pr_feedback_check(plan: &CodexOpenPlan) {
    let Some(query) = plan
        .query
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    if !looks_like_pr_feedback_query(query) {
        return;
    }
    if env_flag_is_false("FLOW_OPEN_CURSOR_ON_PR_CHECK") {
        return;
    }
    let Some(handoff) = plan
        .references
        .iter()
        .find(|reference| reference.name == "pr-feedback")
        .and_then(|reference| parse_pr_feedback_cursor_handoff(&reference.output))
    else {
        return;
    };
    let _ = open_cursor_review_handoff(&handoff);
}

fn env_flag_is_false(name: &str) -> bool {
    let Ok(value) = env::var(name) else {
        return false;
    };
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

fn parse_pr_feedback_cursor_handoff(value: &str) -> Option<PrFeedbackCursorHandoff> {
    let mut workspace_path = None;
    let mut review_plan_path = None;
    let mut review_rules_path = None;
    let mut kit_system_path = None;
    for line in value.lines().map(str::trim) {
        if let Some(path) = line.strip_prefix("Workspace:") {
            workspace_path = Some(PathBuf::from(path.trim()));
        } else if let Some(path) = line.strip_prefix("Review plan:") {
            review_plan_path = Some(PathBuf::from(path.trim()));
        } else if let Some(path) = line.strip_prefix("Review rules:") {
            review_rules_path = Some(PathBuf::from(path.trim()));
        } else if let Some(path) = line.strip_prefix("Kit system prompt:") {
            kit_system_path = Some(PathBuf::from(path.trim()));
        }
    }
    Some(PrFeedbackCursorHandoff {
        workspace_path: workspace_path?,
        review_plan_path: review_plan_path?,
        review_rules_path,
        kit_system_path: kit_system_path?,
    })
}

fn command_on_path(command: &str) -> bool {
    let Some(path_os) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path_os).any(|dir| dir.join(command).is_file())
}

fn open_cursor_review_handoff(handoff: &PrFeedbackCursorHandoff) -> Result<()> {
    let mut command = if cfg!(target_os = "macos") || !command_on_path("cursor") {
        let mut command = Command::new("open");
        command.arg("-g").arg("-a").arg("Cursor");
        command
    } else {
        Command::new("cursor")
    };
    command
        .arg(&handoff.workspace_path)
        .arg(&handoff.review_plan_path)
        .args(handoff.review_rules_path.iter())
        .arg(&handoff.kit_system_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _status = command.status()?;
    Ok(())
}

fn print_codex_open_plan(plan: &CodexOpenPlan) {
    println!("# codex resolve");
    println!("action: {}", plan.action);
    println!("route: {}", plan.route);
    println!("reason: {}", plan.reason);
    println!("target: {}", plan.target_path);
    println!("launch: {}", plan.launch_path);
    println!(
        "budget: {} chars, up to {} reference(s)",
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
        trace_id: plan.trace.as_ref().map(|trace| trace.trace_id.clone()),
        span_id: plan.trace.as_ref().map(|trace| trace.span_id.clone()),
        parent_span_id: plan
            .trace
            .as_ref()
            .and_then(|trace| trace.parent_span_id.clone()),
        workflow_kind: plan.trace.as_ref().map(|trace| trace.workflow_kind.clone()),
        service_name: plan.trace.as_ref().map(|trace| trace.service_name.clone()),
    };

    let _ = codex_skill_eval::log_event(&event);
    let mut activity_event =
        activity_log::ActivityEvent::done(format!("codex.{mode}"), query.to_string());
    activity_event.route = Some(plan.route.clone());
    activity_event.target_path = Some(plan.target_path.clone());
    activity_event.launch_path = Some(plan.launch_path.clone());
    activity_event.session_id = plan.session_id.clone();
    activity_event.runtime_token = plan.runtime_state_path.as_deref().and_then(|path| {
        Path::new(path)
            .file_stem()
            .and_then(|value| value.to_str())
            .map(|value| value.to_string())
    });
    activity_event.source = Some("codex-open-plan".to_string());
    let _ = activity_log::append_daily_event(activity_event);
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

fn looks_like_commit_workflow_query(normalized_query: &str) -> bool {
    let collapsed = normalized_query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if matches!(
        collapsed.as_str(),
        "commit"
            | "commit this"
            | "commit these"
            | "commit it"
            | "commit now"
            | "commit please"
            | "commit and push"
            | "commit & push"
            | "commit/push"
            | "review and commit"
            | "review commit"
            | "review, commit, and push"
    ) {
        return true;
    }

    let Some(rest) = collapsed.strip_prefix("commit ") else {
        return false;
    };
    let blocked_prefixes = [
        "queue",
        "routing",
        "hash",
        "sha",
        "history",
        "log",
        "logs",
        "semantics",
        "title",
        "titles",
        "message",
        "messages",
    ];
    if blocked_prefixes
        .iter()
        .any(|prefix| rest.starts_with(prefix))
    {
        return false;
    }

    if rest.split_whitespace().count() == 1 && rest.len() >= 3 {
        return true;
    }

    let review_cues = [
        "diff",
        "review",
        "inspect",
        "analy",
        "check",
        "push",
        "repo",
        "branch",
        "status",
        "robust",
        "perf",
        "performance",
        "regression",
        "~/",
        "/",
    ];
    review_cues.iter().any(|cue| rest.contains(cue))
}

fn looks_like_sync_workflow_query(normalized_query: &str) -> bool {
    let collapsed = normalized_query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    matches!(
        collapsed.as_str(),
        "sync branch" | "sync this branch" | "sync with origin/main" | "sync with origin main"
    )
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
    scope: CodexFindScope,
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
        resolve_directional_session_lookup(target_path, exact_cwd, normalized_query, scope)?
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
        let rows =
            search_codex_threads_for_find(Some(target_path), exact_cwd, query_text, 1, scope)?;
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
    scope: CodexFindScope,
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
        search_codex_threads_for_find(Some(target_path), exact_cwd, &anchor_text, 1, scope)?
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
        trace: None,
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
        let model = codex_model_label(
            candidate.model.as_deref(),
            candidate.reasoning_effort.as_deref(),
        );
        let line = if let Some(model) = model {
            format!(
                "- {} | {} | {} | {} | {}",
                truncate_recover_id(&candidate.id),
                candidate.updated_at,
                model,
                candidate.cwd,
                preview
            )
        } else {
            format!(
                "- {} | {} | {} | {}",
                truncate_recover_id(&candidate.id),
                candidate.updated_at,
                candidate.cwd,
                preview
            )
        };
        lines.push(line);
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

    if let Some(reference) =
        resolve_builtin_url_reference(target_path, query_text, &candidates, &matches)
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
    query_text: &str,
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
        if looks_like_github_pr_url(candidate) && looks_like_pr_feedback_query(query_text) {
            let Ok(output) = crate::commit::resolve_pr_feedback_reference(target_path, candidate)
            else {
                continue;
            };
            return Some(CodexResolvedReference {
                name: "pr-feedback".to_string(),
                source: "builtin".to_string(),
                matched: candidate.clone(),
                command: Some(format!("f pr feedback {}", shell_words::quote(candidate))),
                output: compact_codex_context_block(&output, 16, 2400),
            });
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

fn resolve_builtin_commit_workflow_reference(repo_root: &Path) -> Result<CodexResolvedReference> {
    let status = capture_git_stdout(repo_root, &["status", "--short"]).unwrap_or_default();
    let staged_diff = capture_git_stdout(
        repo_root,
        &["diff", "--cached", "--stat", "--compact-summary"],
    )
    .unwrap_or_default();
    let working_diff = if staged_diff.trim().is_empty() {
        capture_git_stdout(repo_root, &["diff", "--stat", "--compact-summary"]).unwrap_or_default()
    } else {
        String::new()
    };
    let review_instructions = crate::commit::get_review_instructions(repo_root).unwrap_or_default();
    let agents_instructions = read_repo_agents_instructions(repo_root).unwrap_or_default();
    let kit_gate = detect_commit_workflow_kit_gate(repo_root);
    let output = render_commit_workflow_reference(
        repo_root,
        &status,
        &staged_diff,
        &working_diff,
        &review_instructions,
        &agents_instructions,
        kit_gate.as_deref(),
    );

    Ok(CodexResolvedReference {
        name: "commit-workflow".to_string(),
        source: "builtin".to_string(),
        matched: "commit".to_string(),
        command: Some("f commit --slow --context".to_string()),
        output: compact_codex_context_block(&output, 20, 2200),
    })
}

fn resolve_builtin_sync_workflow_reference(
    repo_root: &Path,
    command: &str,
) -> Result<CodexResolvedReference> {
    let agents_instructions = read_repo_agents_instructions(repo_root).unwrap_or_default();
    let output = render_sync_workflow_reference(repo_root, &agents_instructions, command);

    Ok(CodexResolvedReference {
        name: "sync-workflow".to_string(),
        source: "builtin".to_string(),
        matched: "sync branch".to_string(),
        command: Some(command.to_string()),
        output: compact_codex_context_block(&output, 18, 1600),
    })
}

fn capture_git_stdout(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
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
    Some(trimmed.to_string())
}

fn render_commit_workflow_reference(
    repo_root: &Path,
    status: &str,
    staged_diff: &str,
    working_diff: &str,
    review_instructions: &str,
    agents_instructions: &str,
    kit_gate: Option<&str>,
) -> String {
    let mut lines = vec![
        "Commit workflow contract:".to_string(),
        format!("Workspace: {}", repo_root.display()),
        "Interpret plain `commit` as deep-review-then-commit, not the fast lane.".to_string(),
        "Primary workflow skill: `commit` from the run control plane when it is active. Treat this built-in block as the bootstrap contract, not the only source of truth.".to_string(),
        "If the user names another repo in the query, let the `commit` skill resolve that repo and run the review there instead of assuming this workspace is final.".to_string(),
        "If you use Flow CLI for the final commit, prefer `f commit --slow --context` over plain `f commit`.".to_string(),
        "Default focus: correctness, regression risk, performance, robustness, and clear intent.".to_string(),
        "Preferred execution shape: keep the main thread lean and, if available, use a detached Codex review lane or subagent to inspect the diff in parallel and only surface blocking issues back to the main thread.".to_string(),
        "Treat repo AGENTS.md and repo review instructions as binding commit constraints.".to_string(),
    ];

    if let Some(kit_gate) = kit_gate {
        lines.push(format!("Deterministic gate: {}", kit_gate));
    }

    lines.extend([
        "Required operating order:".to_string(),
        "1. Inspect the actual local diff and adjacent call sites before deciding anything.".to_string(),
        "2. Run deterministic local gates before the final commit when they are available.".to_string(),
        "3. Explain the intent behind the change and the main risks.".to_string(),
        "4. Name the smallest validation that proves the change is safe.".to_string(),
        "5. Draft a commit title/body that explains why the change was made, not just what changed.".to_string(),
        "6. Only commit/push once the review is clean and the change is scoped.".to_string(),
    ]);

    if !status.trim().is_empty() {
        lines.push(String::new());
        lines.push("Git status:".to_string());
        lines.push(render_compact_bullet_block(status, 10));
    }

    if !staged_diff.trim().is_empty() {
        lines.push(String::new());
        lines.push("Staged diff stat:".to_string());
        lines.push(render_compact_bullet_block(staged_diff, 12));
    } else if !working_diff.trim().is_empty() {
        lines.push(String::new());
        lines.push("Working tree diff stat (nothing staged yet):".to_string());
        lines.push(render_compact_bullet_block(working_diff, 12));
    } else {
        lines.push(String::new());
        lines.push("Diff state: working tree is clean right now.".to_string());
    }

    if !review_instructions.trim().is_empty() {
        lines.push(String::new());
        lines.push("Repo commit review instructions:".to_string());
        lines.push(compact_codex_context_block(
            review_instructions.trim(),
            5,
            500,
        ));
    }

    lines.push(String::new());
    lines.push("Final deliverable contract:".to_string());
    lines.push("- provide one short review summary covering correctness, perf, robustness, and regression risk".to_string());
    lines.push("- provide exact validation commands or manual checks".to_string());
    lines.push("- provide the final commit title and body with explicit intent, not only file-level changes".to_string());

    let _ = agents_instructions;
    lines.join("\n")
}

fn render_sync_workflow_reference(
    repo_root: &Path,
    agents_instructions: &str,
    command: &str,
) -> String {
    let mut lines = vec![
        "Sync workflow contract:".to_string(),
        format!("Workspace: {}", repo_root.display()),
        "Interpret plain `sync branch` as the guarded repo sync workflow, not raw `git pull`, generic rebase steps, or improvised JJ commands.".to_string(),
        format!("Preferred command: {}", command),
        "Required operating order:".to_string(),
        "1. Read ./AGENTS.md if it exists and treat repo workflow instructions as binding.".to_string(),
        "2. Use the guarded sync path for this repo instead of ad hoc Git/JJ commands.".to_string(),
        "3. Preserve branch-aware behavior and explain what changed.".to_string(),
        "4. If sync fails, report the blocker and next safe step instead of improvising.".to_string(),
        "Final deliverable contract:".to_string(),
        "- state whether sync succeeded".to_string(),
        "- summarize the main changes pulled in".to_string(),
        "- name any remaining blocker or follow-up".to_string(),
    ];

    if !agents_instructions.trim().is_empty() {
        lines.push(String::new());
        lines.push("Repo workflow instructions:".to_string());
        lines.push(compact_codex_context_block(
            agents_instructions.trim(),
            5,
            400,
        ));
    }

    lines.join("\n")
}

fn render_compact_bullet_block(value: &str, max_lines: usize) -> String {
    let mut lines = Vec::new();
    for line in value.lines().map(str::trim).filter(|line| !line.is_empty()) {
        lines.push(format!("- {}", truncate_message(line, 140)));
        if lines.len() >= max_lines {
            break;
        }
    }
    if lines.is_empty() {
        "- none".to_string()
    } else {
        lines.join("\n")
    }
}

fn read_repo_agents_instructions(repo_root: &Path) -> Option<String> {
    let path = repo_root.join("AGENTS.md");
    let content = fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn detect_commit_workflow_kit_gate(repo_root: &Path) -> Option<String> {
    if !command_on_path("kit") {
        return None;
    }
    Some(format!(
        "cd {} && kit lint --setup never && kit review --dir . --json",
        shell_words::quote(&repo_root.display().to_string())
    ))
}

fn detect_sync_workflow_command(repo_root: &Path) -> Option<String> {
    let cfg = load_codex_config_for_path(repo_root);
    cfg.sync_workflow_command
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
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

fn looks_like_github_pr_url(value: &str) -> bool {
    let trimmed = trim_reference_token(value).trim_end_matches('/');
    let Some(rest) = trimmed.strip_prefix("https://github.com/") else {
        return false;
    };
    let mut parts = rest.split('/');
    let owner = parts.next().unwrap_or_default().trim();
    let repo = parts.next().unwrap_or_default().trim();
    let kind = parts.next().unwrap_or_default().trim();
    let number = parts.next().unwrap_or_default().trim();
    !owner.is_empty() && !repo.is_empty() && kind == "pull" && number.parse::<u64>().is_ok()
}

fn looks_like_pr_feedback_query(query_text: &str) -> bool {
    let lowered = query_text.to_ascii_lowercase();
    lowered.contains("check ")
        || lowered.starts_with("check")
        || lowered.contains("feedback")
        || lowered.contains("comments")
        || lowered.contains("review")
        || lowered.contains("lint")
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
        let header = format!("[{}]", reference.name);
        if !reference.output.trim_start().starts_with(&header) {
            lines.push(header);
        }
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

fn new_workflow_trace_id() -> String {
    Uuid::new_v4().simple().to_string()
}

fn new_workflow_span_id() -> String {
    Uuid::new_v4().simple().to_string()[..16].to_string()
}

fn workflow_kind_from_route(route: &str) -> String {
    route
        .trim()
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-')
        .replace('-', "_")
}

fn trace_context_from_reference(
    reference: &CodexResolvedReference,
    workflow_kind: String,
) -> Option<CodexResolveWorkflowTrace> {
    let fields = parse_reference_fields(&reference.output);
    let trace_id = fields.get("trace id")?.trim();
    if trace_id.is_empty() {
        return None;
    }
    Some(CodexResolveWorkflowTrace {
        trace_id: trace_id.to_string(),
        span_id: new_workflow_span_id(),
        parent_span_id: None,
        workflow_kind,
        service_name: FLOW_CODEX_TRACE_SERVICE_NAME.to_string(),
    })
}

fn derive_codex_open_plan_trace(plan: &CodexOpenPlan) -> Option<CodexResolveWorkflowTrace> {
    if let Some(reference) = plan
        .references
        .iter()
        .find(|reference| reference.name == "pr-feedback")
        .and_then(|reference| trace_context_from_reference(reference, "pr_feedback".to_string()))
    {
        return Some(reference);
    }

    Some(CodexResolveWorkflowTrace {
        trace_id: new_workflow_trace_id(),
        span_id: new_workflow_span_id(),
        parent_span_id: None,
        workflow_kind: workflow_kind_from_route(&plan.route),
        service_name: FLOW_CODEX_TRACE_SERVICE_NAME.to_string(),
    })
}

fn finalize_codex_open_plan(mut plan: CodexOpenPlan) -> CodexOpenPlan {
    if plan.trace.is_none() {
        plan.trace = derive_codex_open_plan_trace(&plan);
    }
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
    use std::process::Command;
    use tempfile::tempdir;

    fn init_temp_git_repo() -> tempfile::TempDir {
        let root = tempdir().expect("tempdir");
        let status = Command::new("git")
            .args(["init"])
            .current_dir(root.path())
            .status()
            .expect("git init");
        assert!(status.success());
        root
    }

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

        let context_packet = concat!(
            "Context packet:\n",
            "- agents: ci, designer\n\n",
            "User request:\n",
            "State your role in one sentence.\n"
        );
        assert_eq!(
            normalize_session_message("user", context_packet).as_deref(),
            Some("State your role in one sentence.")
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
    fn fast_path_codex_connect_only_for_empty_exact_non_json() {
        assert!(should_fast_path_codex_connect("", true, false));
        assert!(should_fast_path_codex_connect("   ", true, false));
        assert!(!should_fast_path_codex_connect(
            "resume latest",
            true,
            false
        ));
        assert!(!should_fast_path_codex_connect("", false, false));
        assert!(!should_fast_path_codex_connect("", true, true));
    }

    #[test]
    fn select_codex_state_db_path_prefers_highest_version() {
        let root = tempdir().expect("tempdir");
        fs::write(root.path().join("state_3.sqlite"), "").expect("write state_3");
        fs::write(root.path().join("state_5.sqlite"), "").expect("write state_5");
        fs::write(root.path().join("state_4.sqlite"), "").expect("write state_4");

        let selected = select_codex_state_db_path(root.path()).expect("select state db");
        assert_eq!(selected, root.path().join("state_5.sqlite"));
    }

    #[test]
    fn read_codex_thread_schema_detects_optional_columns() {
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch(
            r#"
create table threads (
  id text primary key,
  updated_at integer not null,
  cwd text not null,
  title text,
  first_user_message text,
  git_branch text
);
"#,
        )
        .expect("create threads table");

        let initial = read_codex_thread_schema(&conn).expect("read initial schema");
        assert_eq!(
            initial,
            CodexThreadSchema {
                has_rollout_path: false,
                has_model: false,
                has_reasoning_effort: false,
            }
        );

        conn.execute_batch(
            r#"
alter table threads add column model text;
alter table threads add column reasoning_effort text;
"#,
        )
        .expect("alter threads table");

        let updated = read_codex_thread_schema(&conn).expect("read updated schema");
        assert_eq!(
            updated,
            CodexThreadSchema {
                has_rollout_path: false,
                has_model: true,
                has_reasoning_effort: true,
            }
        );
    }

    #[test]
    fn codex_transcript_match_score_finds_phrase_in_assistant_message() {
        let root = tempdir().expect("tempdir");
        let session_file = root.path().join("codex.jsonl");
        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-24T15:37:50Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"explain rust-analyzer server design\"}]}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-24T15:37:54Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"- keep LSP/JSON conversion at the outer boundary only\\n- maintain one mutable server state\"}]}}\n"
            ),
        )
        .expect("write session file");

        let score = codex_transcript_match_score(
            &session_file,
            "keep lsp/json conversion",
            &codex_find_search_terms("keep lsp/json conversion"),
        )
        .expect("score transcript");
        assert!(score >= 900);
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
                rollout_path: None,
                updated_at: 10,
                cwd: "/tmp/repo".to_string(),
                title: Some("one remaining unrelated issue".to_string()),
                first_user_message: Some("npm run lint still fails".to_string()),
                git_branch: Some("main".to_string()),
                model: None,
                reasoning_effort: None,
            },
            CodexRecoverRow {
                id: "019cdcff-0b3a-7a80-b22b-5ac4ff076eff".to_string(),
                rollout_path: None,
                updated_at: 5,
                cwd: "/tmp/other".to_string(),
                title: Some("something else".to_string()),
                first_user_message: Some("different prompt".to_string()),
                git_branch: Some("feature".to_string()),
                model: None,
                reasoning_effort: None,
            },
        ];

        rank_recover_rows(&mut rows, Some("019cdcff"), None, false);

        assert_eq!(rows[0].id, "019cdcff-0b3a-7a80-b22b-5ac4ff076eff");
    }

    #[test]
    fn rank_recover_rows_prefers_recent_match_when_scores_tie() {
        let now_unix = codex_find_now_unix();
        let mut rows = vec![
            CodexRecoverRow {
                id: "019caaaa-0000-7000-8000-aaaaaaaaaaaa".to_string(),
                rollout_path: None,
                updated_at: now_unix - (14 * 24 * 60 * 60),
                cwd: "/tmp/repo".to_string(),
                title: Some("thread read planner".to_string()),
                first_user_message: Some("explain thread read flow".to_string()),
                git_branch: Some("main".to_string()),
                model: None,
                reasoning_effort: None,
            },
            CodexRecoverRow {
                id: "019cbbbb-0000-7000-8000-bbbbbbbbbbbb".to_string(),
                rollout_path: None,
                updated_at: now_unix - (2 * 24 * 60 * 60),
                cwd: "/tmp/repo".to_string(),
                title: Some("thread read planner".to_string()),
                first_user_message: Some("explain thread read flow".to_string()),
                git_branch: Some("main".to_string()),
                model: None,
                reasoning_effort: None,
            },
        ];

        rank_recover_rows(&mut rows, Some("thread read"), None, false);

        assert_eq!(rows[0].id, "019cbbbb-0000-7000-8000-bbbbbbbbbbbb");
    }

    #[test]
    fn rank_recover_rows_prefers_exact_target_path_over_descendant() {
        let now_unix = codex_find_now_unix();
        let target_path = Path::new("/tmp/run");
        let mut rows = vec![
            CodexRecoverRow {
                id: "019caaaa-0000-7000-8000-aaaaaaaaaaaa".to_string(),
                rollout_path: None,
                updated_at: now_unix - (3 * 24 * 60 * 60),
                cwd: "/tmp/run".to_string(),
                title: Some("ci/cd designer".to_string()),
                first_user_message: Some("plan ci/cd designer rollout".to_string()),
                git_branch: Some("main".to_string()),
                model: None,
                reasoning_effort: None,
            },
            CodexRecoverRow {
                id: "019cbbbb-0000-7000-8000-bbbbbbbbbbbb".to_string(),
                rollout_path: None,
                updated_at: now_unix - (60 * 60),
                cwd: "/tmp/run/ide/designer".to_string(),
                title: Some("ci/cd designer".to_string()),
                first_user_message: Some("plan ci/cd designer rollout".to_string()),
                git_branch: Some("main".to_string()),
                model: None,
                reasoning_effort: None,
            },
        ];

        rank_recover_rows(&mut rows, Some("ci/cd designer"), Some(target_path), false);

        assert_eq!(rows[0].id, "019caaaa-0000-7000-8000-aaaaaaaaaaaa");
    }

    #[test]
    fn rank_recover_rows_prefers_structured_query_token_match() {
        let now_unix = codex_find_now_unix();
        let target_path = Path::new("/tmp/run");
        let mut rows = vec![
            CodexRecoverRow {
                id: "019caaaa-0000-7000-8000-aaaaaaaaaaaa".to_string(),
                rollout_path: None,
                updated_at: now_unix - (10 * 24 * 60 * 60),
                cwd: "/tmp/run".to_string(),
                title: Some("ci/cd rollout".to_string()),
                first_user_message: Some(
                    "ci/cd in both mac mini and github action mac minis".to_string(),
                ),
                git_branch: Some("main".to_string()),
                model: None,
                reasoning_effort: None,
            },
            CodexRecoverRow {
                id: "019cbbbb-0000-7000-8000-bbbbbbbbbbbb".to_string(),
                rollout_path: None,
                updated_at: now_unix - (60 * 60),
                cwd: "/tmp/run".to_string(),
                title: Some("designer agent inventory".to_string()),
                first_user_message: Some("designer agent summary for run".to_string()),
                git_branch: Some("main".to_string()),
                model: None,
                reasoning_effort: None,
            },
        ];

        rank_recover_rows(&mut rows, Some("ci/cd designer"), Some(target_path), false);

        assert_eq!(rows[0].id, "019caaaa-0000-7000-8000-aaaaaaaaaaaa");
    }

    #[test]
    fn merge_index_find_matches_demotes_index_only_noise() {
        let now_unix = codex_find_now_unix();
        let target_path = Path::new("/tmp/run");
        let metadata_row = CodexRecoverRow {
            id: "019caaaa-0000-7000-8000-aaaaaaaaaaaa".to_string(),
            rollout_path: None,
            updated_at: now_unix - (2 * 24 * 60 * 60),
            cwd: "/tmp/run".to_string(),
            title: Some("ci/cd rollout".to_string()),
            first_user_message: Some(
                "ci/cd in both mac mini and github action mac minis".to_string(),
            ),
            git_branch: Some("main".to_string()),
            model: None,
            reasoning_effort: None,
        };
        let noisy_row = CodexRecoverRow {
            id: "019cbbbb-0000-7000-8000-bbbbbbbbbbbb".to_string(),
            rollout_path: None,
            updated_at: now_unix - (60 * 60),
            cwd: "/tmp/run".to_string(),
            title: Some("check how many agents we have now defined in run".to_string()),
            first_user_message: Some(
                "check how many agents we have now defined in run".to_string(),
            ),
            git_branch: Some("main".to_string()),
            model: None,
            reasoning_effort: None,
        };

        let rows = merge_index_find_matches(
            vec![
                codex_session_index::CodexSessionIndexHit {
                    row: noisy_row.clone(),
                    score: 2400,
                },
                codex_session_index::CodexSessionIndexHit {
                    row: metadata_row.clone(),
                    score: 900,
                },
            ],
            vec![metadata_row, noisy_row],
            "ci/cd designer",
            now_unix,
            Some(target_path),
            false,
        );

        assert_eq!(rows[0].id, "019caaaa-0000-7000-8000-aaaaaaaaaaaa");
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

        assert_eq!(
            request.session_hints,
            vec!["019ce6ce-c77a-7d52-838e-c01f8820f6b8".to_string()]
        );
        assert_eq!(request.count, 20);
        assert_eq!(request.user_request, "research react hot reload");
    }

    #[test]
    fn extract_codex_session_reference_request_supports_two_session_hints() {
        let request = extract_codex_session_reference_request(
            "see 019cf695-d1d8-7e32-a572-f05e1d03d24f and 019cf983-79c3-7ad0-a870-05e308daa032 codex lets make dedicated plan for /tmp/review.md",
            "see 019cf695-d1d8-7e32-a572-f05e1d03d24f and 019cf983-79c3-7ad0-a870-05e308daa032 codex lets make dedicated plan for /tmp/review.md",
        )
        .expect("expected session reference request");

        assert_eq!(
            request.session_hints,
            vec![
                "019cf695-d1d8-7e32-a572-f05e1d03d24f".to_string(),
                "019cf983-79c3-7ad0-a870-05e308daa032".to_string()
            ]
        );
        assert_eq!(
            request.user_request,
            "lets make dedicated plan for /tmp/review.md"
        );
    }

    #[test]
    fn extract_codex_session_reference_request_supports_suffix_reference_form() {
        let request = extract_codex_session_reference_request(
            "study /tmp/review-sync-plan.md from 019d035d-99b3-7461-9f15-73306348aa28",
            "study /tmp/review-sync-plan.md from 019d035d-99b3-7461-9f15-73306348aa28",
        )
        .expect("expected session reference request");

        assert_eq!(
            request.session_hints,
            vec!["019d035d-99b3-7461-9f15-73306348aa28".to_string()]
        );
        assert_eq!(request.user_request, "study /tmp/review-sync-plan.md");
        assert_eq!(request.count, 12);
    }

    #[test]
    fn extract_codex_session_reference_request_supports_suffix_reference_with_count() {
        let request = extract_codex_session_reference_request(
            "study /tmp/review.md from 019ce6ce-c77a-7d52-838e-c01f8820f6b8 last 7 messages",
            "study /tmp/review.md from 019ce6ce-c77a-7d52-838e-c01f8820f6b8 last 7 messages",
        )
        .expect("expected session reference request");

        assert_eq!(
            request.session_hints,
            vec!["019ce6ce-c77a-7d52-838e-c01f8820f6b8".to_string()]
        );
        assert_eq!(request.user_request, "study /tmp/review.md");
        assert_eq!(request.count, 7);
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
                rollout_path: None,
                updated_at: 5,
                cwd: "/tmp/other".to_string(),
                title: Some("something else".to_string()),
                first_user_message: Some("different prompt".to_string()),
                git_branch: Some("feature".to_string()),
                model: None,
                reasoning_effort: None,
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
            "https://linear.app/fl2024008/project/llm-proxy-v1-6cd0a041bd76/overview"
        ));
        assert!(wildcard_match(
            "https://linear.app/*/issue/*",
            "https://linear.app/fl2024008/issue/IDE-331/test-title"
        ));
        assert!(!wildcard_match(
            "https://linear.app/*/issue/*",
            "https://github.com/openai/codex"
        ));
    }

    #[test]
    fn parse_run_agent_list_output_filters_empty_lines() {
        let parsed = parse_run_agent_list_output(b"commit\n\nplanner\nrun\n");
        assert_eq!(parsed, vec!["commit", "planner", "run"]);
    }

    #[test]
    fn parse_run_agent_completed_event_extracts_final_completed_line() {
        let stdout = br#"{"type":"started","agent_id":"planner"}
{"type":"completed","agent_id":"planner","invocation_id":"20260324T131353Z","artifact_path":"/tmp/plan.md","handoff":{"summary":"ok","nextAction":"wait","artifacts":["/tmp/plan.md"],"relevantPaths":[],"validation":[],"openQuestions":[],"source":"agent"},"output":"flow bridge ok","thread_id":"019d1c41-3797-7e20-b4ad-4e72e68579e6","trace_path":"/tmp/trace.json"}
"#;
        let event = parse_run_agent_completed_event(stdout).expect("completed event");
        assert_eq!(event.event_type, "completed");
        assert_eq!(event.agent_id, "planner");
        assert_eq!(event.output, "flow bridge ok");
        assert_eq!(event.artifact_path.as_deref(), Some("/tmp/plan.md"));
        assert_eq!(
            event
                .handoff
                .as_ref()
                .map(|handoff| handoff.summary.as_str()),
            Some("ok")
        );
        assert_eq!(event.trace_path.as_deref(), Some("/tmp/trace.json"));
    }

    fn sample_codex_doctor_snapshot() -> CodexDoctorSnapshot {
        CodexDoctorSnapshot {
            target: "/tmp/repo".to_string(),
            codex_bin: "codex-flow-wrapper".to_string(),
            codexd: "running".to_string(),
            codexd_socket: "/tmp/codexd.sock".to_string(),
            run_agent_bridge: "ready".to_string(),
            run_agent_router: "/tmp/run/scripts/agent-router.sh".to_string(),
            run_agent_count: 14,
            run_agent_bridge_error: None,
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
    fn codex_eval_opportunities_flag_missing_runtime_and_daemon() {
        let mut snapshot = sample_codex_doctor_snapshot();
        snapshot.runtime_transport = "disabled".to_string();
        snapshot.runtime_skills = "disabled".to_string();
        snapshot.runtime_ready = false;
        snapshot.codexd = "stopped".to_string();
        snapshot.skill_eval_outcomes_on_disk = 0;
        snapshot.learning_ready = false;

        let opportunities = build_codex_eval_opportunities(&snapshot, 4, 0, &[], &[]);
        assert!(
            opportunities
                .iter()
                .any(|item| item.title.contains("Wrapper/runtime path"))
        );
        assert!(
            opportunities
                .iter()
                .any(|item| item.title.contains("codexd is not running"))
        );
        assert!(opportunities.iter().any(|item| {
            item.title
                .contains("No grounded outcome samples for this target yet")
        }));
    }

    #[test]
    fn codex_eval_summary_prefers_grounded_signal_when_ready() {
        let snapshot = sample_codex_doctor_snapshot();
        let route = CodexEvalRouteSnapshot {
            route: "new-with-context".to_string(),
            count: 4,
            share: 0.5,
            avg_context_chars: 420.0,
            avg_reference_count: 1.0,
            runtime_activation_rate: 0.75,
            last_recorded_at_unix: 10,
        };
        let skill = CodexEvalSkillSnapshot {
            name: "github".to_string(),
            score: 12.0,
            sample_size: 4,
            outcome_samples: 3,
            pass_rate: 1.0,
            normalized_gain: 0.4,
            avg_context_chars: 300.0,
        };

        let summary = build_codex_eval_summary(&snapshot, 8, 3, Some(&route), Some(&skill));
        assert!(summary.contains("grounded learning is active"));
        assert!(summary.contains("top route: new-with-context"));
        assert!(summary.contains("top skill: github"));
    }

    #[test]
    fn codex_eval_quality_marks_blocking_runtime_failures_erroneous() {
        let mut snapshot = sample_codex_doctor_snapshot();
        snapshot.runtime_transport = "disabled".to_string();
        snapshot.runtime_skills = "configured-but-inactive".to_string();
        snapshot.runtime_ready = false;
        snapshot.memory_state = "unavailable".to_string();

        let quality = build_codex_eval_quality(&snapshot, 5, 0);
        assert_eq!(quality.status, "erroneous");
        assert!(!quality.grounded);
        assert!(
            quality
                .failure_modes
                .iter()
                .any(|mode| mode.contains("wrapper transport disabled"))
        );
        assert!(
            quality
                .failure_modes
                .iter()
                .any(|mode| mode.contains("runtime skills"))
        );
        assert!(
            quality
                .failure_modes
                .iter()
                .any(|mode| mode.contains("codex memory unavailable"))
        );
    }

    #[test]
    fn codex_eval_quality_stays_valid_while_warming_up() {
        let snapshot = sample_codex_doctor_snapshot();
        let quality = build_codex_eval_quality(&snapshot, 3, 0);
        assert_eq!(quality.status, "valid");
        assert!(!quality.grounded);
        assert!(quality.failure_modes.is_empty());
        assert!(quality.summary.contains("warming up"));
    }

    #[test]
    fn parse_linear_url_reference_extracts_project_shape() {
        let reference = parse_linear_url_reference(
            "https://linear.app/fl2024008/project/llm-proxy-v1-6cd0a041bd76/overview",
        )
        .expect("linear project url should parse");

        assert_eq!(reference.workspace_slug, "fl2024008");
        assert_eq!(reference.resource_kind, LinearUrlKind::Project);
        assert_eq!(reference.resource_value, "llm-proxy-v1-6cd0a041bd76");
        assert_eq!(reference.view.as_deref(), Some("overview"));
        assert_eq!(reference.title_hint, "llm proxy v1");
    }

    #[test]
    fn github_pr_url_detection_is_specific() {
        assert!(looks_like_github_pr_url(
            "https://github.com/fl2024008/prometheus/pull/2922"
        ));
        assert!(!looks_like_github_pr_url(
            "https://github.com/fl2024008/prometheus/issues/2922"
        ));
    }

    #[test]
    fn pr_feedback_query_detection_matches_check_and_comments() {
        assert!(looks_like_pr_feedback_query(
            "check https://github.com/fl2024008/prometheus/pull/2922"
        ));
        assert!(looks_like_pr_feedback_query(
            "see https://github.com/fl2024008/prometheus/pull/2922 for comments"
        ));
        assert!(!looks_like_pr_feedback_query(
            "open https://github.com/fl2024008/prometheus/pull/2922 in browser"
        ));
    }

    #[test]
    fn commit_workflow_query_detection_matches_high_confidence_phrases() {
        assert!(looks_like_commit_workflow_query("commit"));
        assert!(looks_like_commit_workflow_query("commit and push"));
        assert!(looks_like_commit_workflow_query("review and commit"));
        assert!(looks_like_commit_workflow_query("commit flow"));
        assert!(looks_like_commit_workflow_query(
            "commit analyze diff of flow deeply"
        ));
    }

    #[test]
    fn commit_workflow_query_detection_stays_conservative() {
        assert!(!looks_like_commit_workflow_query(
            "improve commit queue throughput"
        ));
        assert!(!looks_like_commit_workflow_query(
            "explain commit routing in flow"
        ));
        assert!(!looks_like_commit_workflow_query("commit hash for release"));
    }

    #[test]
    fn sync_workflow_query_detection_matches_high_confidence_phrases() {
        assert!(looks_like_sync_workflow_query("sync branch"));
        assert!(looks_like_sync_workflow_query("sync this branch"));
        assert!(looks_like_sync_workflow_query("sync with origin/main"));
    }

    #[test]
    fn sync_workflow_query_detection_stays_conservative() {
        assert!(!looks_like_sync_workflow_query(
            "explain sync branch semantics"
        ));
        assert!(!looks_like_sync_workflow_query(
            "sync branch protection settings"
        ));
    }

    #[test]
    fn build_codex_open_plan_routes_plain_commit_into_commit_workflow() {
        let root = init_temp_git_repo();
        fs::write(root.path().join("README.md"), "hello\n").expect("write readme");

        let plan = build_codex_open_plan(
            Some(root.path().display().to_string()),
            vec!["commit".to_string()],
            false,
        )
        .expect("commit plan");

        assert_eq!(plan.route, "commit-workflow-new");
        assert_eq!(plan.action, "new");
        assert_eq!(plan.references[0].name, "commit-workflow");
        assert_eq!(
            plan.references[0].command.as_deref(),
            Some("f commit --slow --context")
        );
        let prompt = plan.prompt.expect("prompt");
        assert!(prompt.contains("Commit workflow contract:"));
        assert!(prompt.contains("deep-review-then-commit"));
        assert!(prompt.contains("Primary workflow skill: `commit`"));
    }

    #[test]
    fn build_codex_open_plan_routes_configured_sync_branch_into_sync_workflow() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo-sync-workspace");
        fs::create_dir_all(&root).expect("create root");
        Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(&root)
            .status()
            .expect("git init");
        fs::write(
            root.join("flow.toml"),
            "version = 1\n\n[codex]\nsync_workflow_command = \"repo sync --safe\"\n",
        )
        .expect("write flow.toml");

        let plan = build_codex_open_plan(
            Some(root.display().to_string()),
            vec!["sync branch".to_string()],
            false,
        )
        .expect("sync plan");

        assert_eq!(plan.route, "sync-workflow-new");
        assert_eq!(plan.action, "new");
        assert_eq!(plan.references[0].name, "sync-workflow");
        assert_eq!(
            plan.references[0].command.as_deref(),
            Some("repo sync --safe")
        );
        let prompt = plan.prompt.expect("prompt");
        assert!(prompt.contains("Sync workflow contract:"));
        assert!(prompt.contains("guarded repo sync workflow"));
    }

    #[test]
    fn build_codex_open_plan_keeps_plain_sync_branch_without_config() {
        let root = init_temp_git_repo();

        let plan = build_codex_open_plan(
            Some(root.path().display().to_string()),
            vec!["sync branch".to_string()],
            false,
        )
        .expect("plan");

        assert_eq!(plan.route, "new-plain");
    }

    #[test]
    fn parse_pr_feedback_cursor_handoff_extracts_paths() {
        let handoff = parse_pr_feedback_cursor_handoff(
            "[pr-feedback]\n\
             Workspace: /tmp/repo\n\
             PR feedback: owner/repo#1\n\
             Review plan: /tmp/plan.md\n\
             Review rules: /tmp/review-rules.md\n\
             Kit system prompt: /tmp/kit.md\n",
        )
        .expect("handoff");

        assert_eq!(handoff.workspace_path, PathBuf::from("/tmp/repo"));
        assert_eq!(handoff.review_plan_path, PathBuf::from("/tmp/plan.md"));
        assert_eq!(
            handoff.review_rules_path,
            Some(PathBuf::from("/tmp/review-rules.md"))
        );
        assert_eq!(handoff.kit_system_path, PathBuf::from("/tmp/kit.md"));
    }

    #[test]
    fn build_codex_prompt_keeps_plain_query_plain() {
        assert_eq!(
            build_codex_prompt("improve codex open perf", &[], 2, 1200).as_deref(),
            Some("improve codex open perf")
        );
    }

    #[test]
    fn build_codex_prompt_avoids_duplicate_reference_header() {
        let references = vec![CodexResolvedReference {
            name: "pr-feedback".to_string(),
            source: "builtin".to_string(),
            matched: "https://github.com/example/repo/pull/1".to_string(),
            command: None,
            output: "[pr-feedback]\nReview plan: /tmp/plan.md".to_string(),
        }];

        let prompt = build_codex_prompt("check pr", &references, 2, 600).expect("prompt");
        assert_eq!(prompt.matches("[pr-feedback]").count(), 1);
    }

    #[test]
    fn parse_reference_fields_extracts_pr_feedback_artifacts() {
        let fields = parse_reference_fields(
            "[pr-feedback]\n\
             Workspace: /tmp/repo\n\
             PR feedback: owner/repo#1\n\
             Trace ID: trace-1\n\
             URL: https://github.com/owner/repo/pull/1\n\
             Snapshot markdown: /tmp/repo/.ai/reviews/pr-feedback-1.md\n\
             Snapshot json: /tmp/repo/.ai/reviews/pr-feedback-1.json\n\
             Review plan: /tmp/plan.md\n\
             Review rules: /tmp/review-rules.md\n\
             Kit system prompt: /tmp/kit.md\n\
             Cursor reopen: f pr feedback https://github.com/owner/repo/pull/1 --compact --cursor\n\
             Summary:\n\
             - Actionable items: 6\n",
        );

        assert_eq!(
            fields.get("workspace").map(String::as_str),
            Some("/tmp/repo")
        );
        assert_eq!(
            fields.get("snapshot markdown").map(String::as_str),
            Some("/tmp/repo/.ai/reviews/pr-feedback-1.md")
        );
        assert_eq!(
            fields.get("review plan").map(String::as_str),
            Some("/tmp/plan.md")
        );
        assert_eq!(fields.get("trace id").map(String::as_str), Some("trace-1"));
        assert_eq!(
            fields.get("cursor reopen").map(String::as_str),
            Some("f pr feedback https://github.com/owner/repo/pull/1 --compact --cursor")
        );
    }

    #[test]
    fn derive_codex_open_plan_trace_assigns_plain_routes() {
        let plan = CodexOpenPlan {
            action: "new".to_string(),
            route: "new-plain".to_string(),
            reason: "start a new session from the current query".to_string(),
            target_path: "/tmp/repo".to_string(),
            launch_path: "/tmp/repo".to_string(),
            query: Some("summarize this repo".to_string()),
            session_id: None,
            prompt: Some("summarize this repo".to_string()),
            references: Vec::new(),
            runtime_state_path: None,
            runtime_skills: Vec::new(),
            prompt_context_budget_chars: 1200,
            max_resolved_references: 3,
            prompt_chars: 19,
            injected_context_chars: 0,
            trace: None,
        };

        let trace = derive_codex_open_plan_trace(&plan).expect("trace");
        assert_eq!(trace.workflow_kind, "new_plain");
        assert_eq!(trace.service_name, FLOW_CODEX_TRACE_SERVICE_NAME);
        assert_eq!(trace.trace_id.len(), 32);
        assert_eq!(trace.span_id.len(), 16);
    }

    #[test]
    fn build_pr_feedback_workflow_explanation_surfaces_packet_and_command() {
        let plan = CodexOpenPlan {
            action: "new".to_string(),
            route: "new-with-context".to_string(),
            reason: "builtin pr feedback route".to_string(),
            target_path: "/tmp/repo".to_string(),
            launch_path: "/tmp/repo".to_string(),
            query: Some("check https://github.com/owner/repo/pull/1".to_string()),
            session_id: None,
            prompt: Some("prompt".to_string()),
            references: vec![CodexResolvedReference {
                name: "pr-feedback".to_string(),
                source: "builtin".to_string(),
                matched: "https://github.com/owner/repo/pull/1".to_string(),
                command: Some("f pr feedback https://github.com/owner/repo/pull/1".to_string()),
                output: "[pr-feedback]\n\
                         Workspace: /tmp/repo\n\
                         PR feedback: owner/repo#1\n\
                         Trace ID: trace-1\n\
                         URL: https://github.com/owner/repo/pull/1\n\
                         Snapshot markdown: /tmp/repo/.ai/reviews/pr-feedback-1.md\n\
                         Snapshot json: /tmp/repo/.ai/reviews/pr-feedback-1.json\n\
                         Review plan: /tmp/plan.md\n\
                         Review rules: /tmp/review-rules.md\n\
                         Kit system prompt: /tmp/kit.md\n\
                         Cursor reopen: f pr feedback https://github.com/owner/repo/pull/1 --compact --cursor\n"
                    .to_string(),
            }],
            runtime_state_path: None,
            runtime_skills: vec![],
            prompt_context_budget_chars: 2400,
            max_resolved_references: 2,
            prompt_chars: 100,
            injected_context_chars: 80,
            trace: Some(CodexResolveWorkflowTrace {
                trace_id: "trace-1".to_string(),
                span_id: "span-1".to_string(),
                parent_span_id: None,
                workflow_kind: "pr_feedback".to_string(),
                service_name: FLOW_CODEX_TRACE_SERVICE_NAME.to_string(),
            }),
        };
        let runtime_skills = vec![CodexResolveRuntimeSkillSnapshot {
            name: "flow-runtime-ext-dimillian-skills-github".to_string(),
            kind: "external".to_string(),
            path: "/tmp/github".to_string(),
            trigger: "github".to_string(),
            source: Some("dimillian".to_string()),
            original_name: Some("github".to_string()),
            estimated_chars: Some(1200),
            match_reason: Some("matched skill name phrase `github`".to_string()),
        }];

        let workflow = build_codex_resolve_workflow_explanation(&plan, &runtime_skills)
            .expect("workflow explanation");
        assert_eq!(workflow.id, "pr-feedback");
        assert_eq!(workflow.packet.kind, "pr_feedback");
        assert!(
            workflow
                .packet
                .expansion_rules
                .iter()
                .any(|rule| rule.contains("Read the compact packet first"))
        );
        assert!(
            workflow
                .packet
                .validation_plan
                .iter()
                .any(|item| item.label == "Per-item product validation")
        );
        assert_eq!(
            workflow.commands.first().map(|c| c.command.as_str()),
            Some("f pr feedback https://github.com/owner/repo/pull/1")
        );
        assert!(workflow
            .artifacts
            .iter()
            .any(|artifact| artifact.label == "Review plan" && artifact.value == "/tmp/plan.md"));
        assert!(
            workflow
                .artifacts
                .iter()
                .any(|artifact| artifact.label == "Trace ID" && artifact.value == "trace-1")
        );
        assert_eq!(
            workflow
                .packet
                .trace
                .as_ref()
                .map(|trace| trace.trace_id.as_str()),
            Some("trace-1")
        );
        assert!(
            workflow
                .notes
                .iter()
                .any(|note| note.contains("Runtime skill: github"))
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
    fn read_codex_session_completion_snapshot_tracks_latest_completed_turn() {
        let root = tempdir().expect("tempdir");
        let session_file = root.path().join("codex.jsonl");
        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T10:00:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"first prompt\"}]}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T10:00:01Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"first answer\"}]}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T10:01:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"second prompt\"}]}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-17T10:01:03Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"second answer\"}]}}\n"
            ),
        )
        .expect("write session file");

        let snapshot = read_codex_session_completion_snapshot(&session_file)
            .expect("snapshot")
            .expect("completion snapshot");
        assert_eq!(snapshot.last_role.as_deref(), Some("assistant"));
        assert_eq!(snapshot.last_user_message.as_deref(), Some("second prompt"));
        assert_eq!(
            snapshot.last_assistant_message.as_deref(),
            Some("second answer")
        );
        assert_eq!(
            snapshot.last_assistant_at_unix,
            parse_rfc3339_to_unix("2026-03-17T10:01:03Z")
        );
    }

    #[test]
    fn select_codex_session_completion_summary_prefers_last_user_message() {
        let row = CodexRecoverRow {
            id: "019ce791-7e05-7e51-b2b7-610dc7172e5c".to_string(),
            rollout_path: None,
            updated_at: 0,
            cwd: "/tmp/repo".to_string(),
            title: Some("fallback title".to_string()),
            first_user_message: Some("older intent".to_string()),
            git_branch: None,
            model: None,
            reasoning_effort: None,
        };
        let snapshot = CodexSessionCompletionSnapshot {
            last_role: Some("assistant".to_string()),
            last_user_message: Some("implement codex session logging".to_string()),
            last_user_at_unix: Some(1),
            last_assistant_message: Some("done".to_string()),
            last_assistant_at_unix: Some(2),
            file_modified_unix: 2,
        };

        let summary = select_codex_session_completion_summary(&row, &snapshot);
        assert_eq!(summary, "implement codex session logging");
    }

    #[test]
    fn parse_apply_patch_changes_extracts_absolute_paths() {
        let changes = parse_apply_patch_changes(
            concat!(
                "*** Begin Patch\n",
                "*** Update File: /tmp/config/fish/fn.fish\n",
                "@@\n",
                "+function j\n",
                "*** Add File: relative/new-file.rs\n",
                "+fn main() {}\n",
                "*** End Patch\n",
            ),
            "/tmp/code/flow",
        );
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "/tmp/config/fish/fn.fish");
        assert_eq!(changes[0].action, "update");
        assert!(changes[0].patch.contains("function j"));
        assert_eq!(changes[1].path, "/tmp/code/flow/relative/new-file.rs");
        assert_eq!(changes[1].action, "add");
    }

    #[test]
    fn summarize_fish_fn_change_detects_shortcut_remap() {
        let summary = summarize_fish_fn_change(
            "j runs f codex open --path (pwd -P) --exact-cwd. \
k uses f codex connect --path (pwd -P) --exact-cwd. \
l is now Kit for ~/repos/mark3labs/kit. \
L now delegates to j. old k moved to cl. old l moved to cf. old L moved to cF.",
        )
        .expect("summary");
        assert!(summary.contains("j->codex.open"));
        assert!(summary.contains("k->codex.connect"));
        assert!(summary.contains("l->kit"));
        assert!(summary.contains("L->j"));
        assert!(summary.contains("cl/cf/cF"));
    }

    #[test]
    fn build_codex_session_changed_events_uses_fish_summary_fallback() {
        let root = tempdir().expect("tempdir");
        let session_file = root.path().join("codex.jsonl");
        fs::write(&session_file, "").expect("write empty session file");

        let row = CodexRecoverRow {
            id: "019ce791-7e05-7e51-b2b7-610dc7172e5c".to_string(),
            rollout_path: None,
            updated_at: 0,
            cwd: "/tmp/code/flow".to_string(),
            title: None,
            first_user_message: None,
            git_branch: None,
            model: None,
            reasoning_effort: None,
        };
        let snapshot = CodexSessionCompletionSnapshot {
            last_role: Some("assistant".to_string()),
            last_user_message: Some(
                "The remap is in fn.fish. j runs f codex open --path (pwd -P) --exact-cwd. \
k uses f codex connect --path (pwd -P) --exact-cwd. \
l is now Kit. L now delegates to j. old k moved to cl. old l moved to cf. old L moved to cF."
                    .to_string(),
            ),
            last_user_at_unix: Some(1),
            last_assistant_message: Some("logged".to_string()),
            last_assistant_at_unix: Some(2),
            file_modified_unix: 2,
        };

        let events =
            build_codex_session_changed_events(&row, &snapshot, &session_file).expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "fish.fn");
        assert!(events[0].summary.contains("j->codex.open"));
        assert!(events[0].summary.contains("k->codex.connect"));
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

    #[test]
    fn ai_session_from_codex_recover_row_prefers_title_for_preview() {
        let session = ai_session_from_codex_recover_row(CodexRecoverRow {
            id: "019ce791-7e05-7e51-b2b7-610dc7172e5c".to_string(),
            rollout_path: None,
            updated_at: 1_773_776_290,
            cwd: "/tmp/repo".to_string(),
            title: Some("review github integration".to_string()),
            first_user_message: Some("older prompt".to_string()),
            git_branch: None,
            model: None,
            reasoning_effort: None,
        });

        assert_eq!(
            session.last_message.as_deref(),
            Some("review github integration")
        );
        assert_eq!(session.first_message.as_deref(), Some("older prompt"));
        assert_eq!(session.provider, Provider::Codex);
        assert!(session.last_message_at.is_some());
    }

    #[test]
    fn ai_session_from_codex_recover_row_falls_back_to_first_user_message() {
        let session = ai_session_from_codex_recover_row(CodexRecoverRow {
            id: "019ce791-7e05-7e51-b2b7-610dc7172e5c".to_string(),
            rollout_path: None,
            updated_at: 1_773_776_290,
            cwd: "/tmp/repo".to_string(),
            title: None,
            first_user_message: Some("inspect the current diff".to_string()),
            git_branch: None,
            model: None,
            reasoning_effort: None,
        });

        assert_eq!(
            session.last_message.as_deref(),
            Some("inspect the current diff")
        );
        assert_eq!(
            session.first_message.as_deref(),
            Some("inspect the current diff")
        );
    }
}
