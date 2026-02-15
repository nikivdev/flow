use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use shellexpand::tilde;

use crate::fixup;

/// Top-level configuration for flowd, currently focused on managed servers.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub version: Option<u32>,
    /// Optional human-friendly project name (applies to local project configs).
    #[serde(
        default,
        rename = "name",
        alias = "project_name",
        alias = "project-name"
    )]
    pub project_name: Option<String>,
    /// Optional env store space override for cloud.
    #[serde(default, rename = "env_space", alias = "env-space")]
    pub env_space: Option<String>,
    /// Env store scope: "project" (default) or "personal".
    #[serde(
        default,
        rename = "env_space_kind",
        alias = "env-space-kind",
        alias = "env-space-scope"
    )]
    pub env_space_kind: Option<String>,
    /// Flow-specific settings (primary_task, etc.)
    #[serde(default)]
    pub flow: FlowSettings,
    #[serde(default)]
    pub options: OptionsConfig,
    #[serde(default, alias = "server", alias = "server-local")]
    pub servers: Vec<ServerConfig>,
    #[serde(default, rename = "server-remote")]
    pub remote_servers: Vec<RemoteServerConfig>,
    #[serde(default)]
    pub tasks: Vec<TaskConfig>,
    /// Skills enforcement configuration (auto-sync/install).
    #[serde(default)]
    pub skills: Option<SkillsConfig>,
    /// Anonymous usage analytics settings.
    #[serde(default)]
    pub analytics: Option<AnalyticsConfig>,
    /// Hive agents defined for this project (array format: [[agent]]).
    #[serde(default, rename = "agent")]
    pub agents: Vec<crate::hive::AgentConfig>,
    /// Agent registry references (map format: [agents]).
    #[serde(default)]
    pub agents_registry: HashMap<String, String>,
    #[serde(default, alias = "deps")]
    pub dependencies: HashMap<String, DependencySpec>,
    #[serde(default, alias = "alias", deserialize_with = "deserialize_aliases")]
    pub aliases: HashMap<String, String>,
    #[serde(default, rename = "commands")]
    pub command_files: Vec<CommandFileConfig>,
    #[serde(default)]
    pub storage: Option<StorageConfig>,
    #[serde(default)]
    pub flox: Option<FloxConfig>,
    #[serde(default, alias = "watcher", alias = "always-run")]
    pub watchers: Vec<WatcherConfig>,
    #[serde(default)]
    pub stream: Option<StreamConfig>,
    #[serde(default, rename = "server-hub")]
    pub server_hub: Option<ServerHubConfig>,
    /// Background daemons that flow can manage (start/stop/status).
    #[serde(default, alias = "daemon")]
    pub daemons: Vec<DaemonConfig>,
    /// Host deployment config for Linux servers.
    #[serde(default)]
    pub host: Option<crate::deploy::HostConfig>,
    /// Cloudflare Workers deployment config.
    #[serde(default)]
    pub cloudflare: Option<crate::deploy::CloudflareConfig>,
    /// Railway deployment config.
    #[serde(default)]
    pub railway: Option<crate::deploy::RailwayConfig>,
    /// Web deployment config.
    #[serde(default)]
    pub web: Option<crate::deploy::WebConfig>,
    /// Production deploy overrides (used by `f prod`).
    #[serde(default, alias = "production")]
    pub prod: Option<crate::deploy::ProdConfig>,
    /// Release configuration (hosts, npm, etc.).
    #[serde(default)]
    pub release: Option<ReleaseConfig>,
    /// Commit workflow config (fixers, review instructions).
    #[serde(default)]
    pub commit: Option<CommitConfig>,
    /// Jujutsu (jj) workflow config.
    #[serde(default)]
    pub jj: Option<JjConfig>,
    /// Setup defaults (global or project-level).
    #[serde(default)]
    pub setup: Option<SetupConfig>,
    /// SSH defaults (global or project-level).
    #[serde(default)]
    pub ssh: Option<SshConfig>,
    /// macOS launchd service management config.
    #[serde(default)]
    pub macos: Option<MacosConfig>,
    /// Proxy server configuration.
    #[serde(default)]
    pub proxy: Option<crate::proxy::ProxyConfig>,
    /// Proxy targets (array format: [[proxies]]).
    #[serde(default, alias = "proxy-target")]
    pub proxies: Vec<crate::proxy::ProxyTargetConfig>,
}

/// macOS launchd service management config.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct MacosConfig {
    /// Service patterns that are allowed (won't be flagged).
    /// Supports wildcards like "com.nikiv.*".
    #[serde(default)]
    pub allowed: Vec<String>,
    /// Service patterns that should be blocked/disabled.
    /// Supports wildcards like "com.google.*".
    #[serde(default)]
    pub blocked: Vec<String>,
}

/// SSH config (mode, key name, etc.).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SshConfig {
    /// ssh mode: "auto", "force", or "https"
    #[serde(default)]
    pub mode: Option<String>,
    /// default key name to unlock (defaults to "default").
    #[serde(default)]
    pub key_name: Option<String>,
    /// auto-unlock ssh keys when needed (default: true).
    #[serde(default)]
    pub auto_unlock: Option<bool>,
}

/// Configuration for commit workflow.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CommitConfig {
    /// Pre-commit fixers to run before staging.
    /// Built-in: "mdx-comments", "trailing-whitespace", "end-of-file"
    /// Custom: "cmd:prettier --write"
    #[serde(default)]
    pub fixers: Vec<String>,
    /// Custom instructions passed to AI code review.
    #[serde(default)]
    pub review_instructions: Option<String>,
    /// File path to load review instructions from.
    #[serde(default)]
    pub review_instructions_file: Option<String>,
    /// Tool to use for commit review: "claude", "codex", "opencode", "kimi"
    #[serde(default)]
    pub tool: Option<String>,
    /// Model to use for commit review (tool-specific)
    #[serde(default)]
    pub model: Option<String>,
    /// Tool to use for commit message generation: "kimi"
    #[serde(
        default,
        rename = "message-tool",
        alias = "message_tool",
        alias = "messageTool"
    )]
    pub message_tool: Option<String>,
    /// Model to use for commit message generation (tool-specific)
    #[serde(
        default,
        rename = "message-model",
        alias = "message_model",
        alias = "messageModel"
    )]
    pub message_model: Option<String>,
    /// Continue commit if review fails after fallbacks (default: true)
    #[serde(
        default,
        rename = "review-fail-open",
        alias = "review_fail_open",
        alias = "reviewFailOpen"
    )]
    pub review_fail_open: Option<bool>,
    /// Continue commit if commit-message generation fails after fallbacks (default: true)
    #[serde(
        default,
        rename = "message-fail-open",
        alias = "message_fail_open",
        alias = "messageFailOpen"
    )]
    pub message_fail_open: Option<bool>,
    /// Optional ordered fallback chain for review tool/model.
    /// Examples: ["openrouter:openrouter/free", "claude", "codex-high"]
    #[serde(
        default,
        rename = "review-fallbacks",
        alias = "review_fallbacks",
        alias = "reviewFallbacks"
    )]
    pub review_fallbacks: Option<Vec<String>>,
    /// Optional ordered fallback chain for commit message generation.
    /// Examples: ["remote", "openai", "openrouter:openrouter/free", "heuristic"]
    #[serde(
        default,
        rename = "message-fallbacks",
        alias = "message_fallbacks",
        alias = "messageFallbacks"
    )]
    pub message_fallbacks: Option<Vec<String>>,
    /// Queue commits for review before push.
    #[serde(default)]
    pub queue: Option<bool>,
    /// Queue only when review finds issues (overrides queue if review passes).
    #[serde(
        default,
        rename = "queue_on_issues",
        alias = "queue-on-issues",
        alias = "queueOnIssues"
    )]
    pub queue_on_issues: Option<bool>,
    /// Quality gate configuration for commit-time feature doc/test enforcement.
    #[serde(default)]
    pub quality: Option<QualityConfig>,
    /// Test-runner enforcement and pre-commit test gate settings.
    #[serde(default)]
    pub testing: Option<TestingConfig>,
    /// Required workflow skills gate for commit-time enforcement.
    #[serde(
        default,
        rename = "skill_gate",
        alias = "skill-gate",
        alias = "skillGate"
    )]
    pub skill_gate: Option<SkillGateConfig>,
}

/// Quality gate configuration: enforce documentation and test requirements at commit time.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct QualityConfig {
    /// Gate mode: "warn" (default) | "block" | "off"
    #[serde(default)]
    pub mode: Option<String>,
    /// Require feature docs for touched features (default: true)
    #[serde(default)]
    pub require_docs: Option<bool>,
    /// Require test files for changed source code (default: true)
    #[serde(default)]
    pub require_tests: Option<bool>,
    /// Auto-generate/update feature docs at commit time (default: true)
    #[serde(default)]
    pub auto_generate_docs: Option<bool>,
    /// Doc detail level: "basic" | "detailed" (default: "basic")
    #[serde(default)]
    pub doc_level: Option<String>,
    /// Glob patterns exempt from quality checks
    #[serde(default)]
    pub exempt_paths: Option<Vec<String>>,
    /// Days before a feature doc is flagged stale (default: 30)
    #[serde(default)]
    pub stale_days: Option<u32>,
}

/// Testing gate configuration: enforce Bun test runner usage and quick local checks.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TestingConfig {
    /// Gate mode: "warn" (default) | "block" | "off"
    #[serde(default)]
    pub mode: Option<String>,
    /// Required runner (currently "bun" only). Default: "bun".
    #[serde(default)]
    pub runner: Option<String>,
    /// In Bun repo layout, require `bun bd test` instead of `bun test`. Default: true.
    #[serde(default)]
    pub bun_repo_strict: Option<bool>,
    /// Require at least one related test for staged source changes. Default: true.
    #[serde(default)]
    pub require_related_tests: Option<bool>,
    /// Directory for AI scratch tests (typically gitignored). Default: ".ai/test".
    #[serde(default)]
    pub ai_scratch_test_dir: Option<String>,
    /// Run AI scratch tests when no related tracked tests are detected. Default: true.
    #[serde(default)]
    pub run_ai_scratch_tests: Option<bool>,
    /// Allow AI scratch tests to satisfy related-test gate requirements. Default: false.
    #[serde(default)]
    pub allow_ai_scratch_to_satisfy_gate: Option<bool>,
    /// Soft budget in seconds for the local test gate; emits warning if exceeded. Default: 15.
    #[serde(default)]
    pub max_local_gate_seconds: Option<u64>,
}

/// Skill gate configuration: require specific workflow skills before commit.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SkillGateConfig {
    /// Gate mode: "warn" | "block" | "off"
    #[serde(default)]
    pub mode: Option<String>,
    /// Required skill names (must exist in .ai/skills).
    #[serde(default)]
    pub required: Vec<String>,
    /// Optional per-skill minimum version (from skill frontmatter "version").
    #[serde(default, rename = "min_version", alias = "min-version")]
    pub min_version: Option<HashMap<String, u32>>,
}

/// Jujutsu (jj) workflow config.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct JjConfig {
    /// Default branch to rebase onto (e.g., "main").
    #[serde(
        default,
        rename = "default_branch",
        alias = "default-branch",
        alias = "defaultBranch"
    )]
    pub default_branch: Option<String>,
    /// Default git remote (e.g., "origin").
    #[serde(default)]
    pub remote: Option<String>,
    /// Auto-track bookmarks on create.
    #[serde(
        default,
        rename = "auto_track",
        alias = "auto-track",
        alias = "autoTrack"
    )]
    pub auto_track: Option<bool>,
    /// Prefix for review bookmarks created by flow (e.g., "review").
    #[serde(
        default,
        rename = "review_prefix",
        alias = "review-prefix",
        alias = "reviewPrefix"
    )]
    pub review_prefix: Option<String>,
}

/// TypeScript config loaded from ~/.config/flow/config.ts
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TsConfig {
    #[serde(default)]
    pub flow: Option<TsFlowConfig>,
}

/// Flow section from TypeScript config.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TsFlowConfig {
    #[serde(default)]
    pub commit: Option<TsCommitConfig>,
    #[serde(default)]
    pub review: Option<TsReviewConfig>,
    #[serde(default)]
    pub agents: Option<TsAgentsConfig>,
    #[serde(default)]
    pub env: Option<TsEnvConfig>,
    #[serde(default, rename = "taskFailureAgents")]
    pub task_failure_agents: Option<TsTaskFailureAgentsConfig>,
    /// Optional command to run on task failure.
    #[serde(
        default,
        rename = "taskFailureHook",
        alias = "task_failure_hook",
        alias = "task-failure-hook"
    )]
    pub task_failure_hook: Option<String>,
    /// Enable gitedit.dev hash in commit messages. Default false.
    #[serde(default)]
    pub gitedit: Option<bool>,
    /// Log level: "off", "error", "warn", "info", "debug", "trace". Default "warn".
    #[serde(default)]
    pub log_level: Option<String>,
}

/// Env settings from TypeScript config.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TsEnvConfig {
    /// Preferred env backend: "cloud" or "local".
    #[serde(default)]
    pub backend: Option<String>,
    /// Env vars to inject into every task from the personal env store.
    #[serde(
        default,
        rename = "global_keys",
        alias = "globalKeys",
        alias = "global-keys"
    )]
    pub global_keys: Vec<String>,
}

/// Agents settings from TypeScript config.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TsAgentsConfig {
    /// Tool to use: "claude", "gen", "opencode"
    #[serde(default)]
    pub tool: Option<String>,
    /// Default model for agents (e.g., "openrouter/moonshotai/kimi-k2:free")
    #[serde(default)]
    pub model: Option<String>,
}

/// Task-failure agent routing settings from TypeScript config.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TsTaskFailureAgentsConfig {
    /// Enable auto-routing on task failure.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Tool to use (currently "hive").
    #[serde(default)]
    pub tool: Option<String>,
    /// Max lines of task output to include in prompt.
    #[serde(default, rename = "maxLines")]
    pub max_lines: Option<usize>,
    /// Max chars of task output to include in prompt.
    #[serde(default, rename = "maxChars")]
    pub max_chars: Option<usize>,
    /// Max agents to run per failure.
    #[serde(default, rename = "maxAgents")]
    pub max_agents: Option<usize>,
}

/// Commit settings from TypeScript config.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TsCommitConfig {
    /// Tool to use: "claude", "codex", "opencode"
    #[serde(default)]
    pub tool: Option<String>,
    /// Model identifier (e.g., "opencode/minimax-m2.1-free")
    #[serde(default)]
    pub model: Option<String>,
    /// Tool to use for commit message generation: "kimi"
    #[serde(
        default,
        rename = "messageTool",
        alias = "message_tool",
        alias = "message-tool"
    )]
    pub message_tool: Option<String>,
    /// Model identifier for commit message generation
    #[serde(
        default,
        rename = "messageModel",
        alias = "message_model",
        alias = "message-model"
    )]
    pub message_model: Option<String>,
    /// Continue commit if review fails after fallbacks (default: true)
    #[serde(
        default,
        rename = "reviewFailOpen",
        alias = "review_fail_open",
        alias = "review-fail-open"
    )]
    pub review_fail_open: Option<bool>,
    /// Continue commit if commit-message generation fails after fallbacks (default: true)
    #[serde(
        default,
        rename = "messageFailOpen",
        alias = "message_fail_open",
        alias = "message-fail-open"
    )]
    pub message_fail_open: Option<bool>,
    /// Optional ordered fallback chain for review.
    #[serde(
        default,
        rename = "reviewFallbacks",
        alias = "review_fallbacks",
        alias = "review-fallbacks"
    )]
    pub review_fallbacks: Option<Vec<String>>,
    /// Optional ordered fallback chain for message generation.
    #[serde(
        default,
        rename = "messageFallbacks",
        alias = "message_fallbacks",
        alias = "message-fallbacks"
    )]
    pub message_fallbacks: Option<Vec<String>>,
    /// Custom review instructions
    #[serde(default)]
    pub review_instructions: Option<String>,
    /// Queue commits for review before push.
    #[serde(default)]
    pub queue: Option<bool>,
    /// Queue only when review finds issues (overrides queue if review passes).
    #[serde(
        default,
        rename = "queueOnIssues",
        alias = "queue_on_issues",
        alias = "queue-on-issues"
    )]
    pub queue_on_issues: Option<bool>,
    /// Whether to run async (delegate to hub). Default true.
    #[serde(default, rename = "async")]
    pub async_enabled: Option<bool>,
}

/// Review settings from TypeScript config (overrides commit settings for review).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TsReviewConfig {
    /// Tool to use for review: "claude", "codex", "opencode", "kimi"
    #[serde(default)]
    pub tool: Option<String>,
    /// Model identifier for review (e.g., "opencode/glm-4.7-free")
    #[serde(default)]
    pub model: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: None,
            project_name: None,
            env_space: None,
            env_space_kind: None,
            flow: FlowSettings::default(),
            options: OptionsConfig::default(),
            servers: Vec::new(),
            remote_servers: Vec::new(),
            tasks: Vec::new(),
            skills: None,
            analytics: None,
            agents: Vec::new(),
            agents_registry: HashMap::new(),
            dependencies: HashMap::new(),
            aliases: HashMap::new(),
            command_files: Vec::new(),
            storage: None,
            flox: None,
            watchers: Vec::new(),
            stream: None,
            server_hub: None,
            daemons: Vec::new(),
            host: None,
            cloudflare: None,
            railway: None,
            web: None,
            prod: None,
            release: None,
            commit: None,
            jj: None,
            setup: None,
            ssh: None,
            macos: None,
            proxy: None,
            proxies: Vec::new(),
        }
    }
}

/// Flow-specific settings for autonomous agent workflows.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FlowSettings {
    /// The primary task to run after code changes (e.g., "release", "deploy").
    #[serde(default, alias = "primary-task")]
    pub primary_task: Option<String>,
    /// Task to run when invoking `f deploy release`.
    #[serde(default, rename = "release_task", alias = "release-task")]
    pub release_task: Option<String>,
    /// Task to run when invoking `f deploy` with no subcommand.
    #[serde(default, rename = "deploy_task", alias = "deploy-task")]
    pub deploy_task: Option<String>,
}

/// Skills enforcement configuration.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SkillsConfig {
    /// Auto-sync flow.toml tasks into .ai/skills.
    #[serde(
        default,
        rename = "sync_tasks",
        alias = "sync-tasks",
        alias = "syncTasks"
    )]
    pub sync_tasks: bool,
    /// Skills to install from the registry when missing.
    #[serde(default)]
    pub install: Vec<String>,
    /// Codex-specific skills behavior.
    #[serde(default)]
    pub codex: Option<SkillsCodexConfig>,
    /// Optional seq scraper integration for dependency skill generation.
    #[serde(default)]
    pub seq: Option<SkillsSeqConfig>,
}

/// Anonymous usage analytics settings.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AnalyticsConfig {
    /// Force analytics enabled/disabled regardless of local prompt state.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Ingest endpoint for analytics events.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Client-side sampling rate (0.0..1.0, default 1.0).
    #[serde(default)]
    pub sample_rate: Option<f32>,
}

/// Codex-focused skills settings.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SkillsCodexConfig {
    /// Generate `agents/openai.yaml` metadata for task-synced skills.
    #[serde(
        default,
        rename = "generate_openai_yaml",
        alias = "generate-openai-yaml",
        alias = "generateOpenaiYaml"
    )]
    pub generate_openai_yaml: Option<bool>,
    /// After sync/install, force Codex app-server to reload skills for this cwd.
    #[serde(
        default,
        rename = "force_reload_after_sync",
        alias = "force-reload-after-sync",
        alias = "forceReloadAfterSync"
    )]
    pub force_reload_after_sync: Option<bool>,
    /// Default implicit invocation policy for task-synced skills metadata.
    #[serde(
        default,
        rename = "task_skill_allow_implicit_invocation",
        alias = "task-skill-allow-implicit-invocation",
        alias = "taskSkillAllowImplicitInvocation"
    )]
    pub task_skill_allow_implicit_invocation: Option<bool>,
}

/// Seq-backed skills fetch configuration.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SkillsSeqConfig {
    /// Fetch mode ("local-cli" today; "remote-api" reserved).
    #[serde(default)]
    pub mode: Option<String>,
    /// Path to seq repo (used to resolve tools/teach_deps.py).
    #[serde(default, rename = "seq_repo", alias = "seq-repo")]
    pub seq_repo: Option<String>,
    /// Full path to teach_deps.py (overrides seq_repo).
    #[serde(default, rename = "script_path", alias = "script-path")]
    pub script_path: Option<String>,
    /// Scraper daemon/API base URL.
    #[serde(
        default,
        rename = "scraper_base_url",
        alias = "scraper-base-url",
        alias = "scraperBaseUrl"
    )]
    pub scraper_base_url: Option<String>,
    /// Scraper bearer token.
    #[serde(
        default,
        rename = "scraper_api_key",
        alias = "scraper-api-key",
        alias = "scraperApiKey"
    )]
    pub scraper_api_key: Option<String>,
    /// Output directory for generated skills.
    #[serde(default, rename = "out_dir", alias = "out-dir")]
    pub out_dir: Option<String>,
    /// Cache TTL in hours.
    #[serde(
        default,
        rename = "cache_ttl_hours",
        alias = "cache-ttl-hours",
        alias = "cacheTtlHours"
    )]
    pub cache_ttl_hours: Option<f64>,
    /// Direct fetch fallback when scraper queue is unavailable.
    #[serde(
        default,
        rename = "allow_direct_fallback",
        alias = "allow-direct-fallback",
        alias = "allowDirectFallback"
    )]
    pub allow_direct_fallback: Option<bool>,
    /// Optional seq.mem JSONEachRow destination path.
    #[serde(
        default,
        rename = "mem_events_path",
        alias = "mem-events-path",
        alias = "memEventsPath"
    )]
    pub mem_events_path: Option<String>,
    /// Default per-ecosystem dependency count for auto mode.
    #[serde(default)]
    pub top: Option<usize>,
    /// Default ecosystems for auto mode.
    #[serde(default)]
    pub ecosystems: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ReleaseConfig {
    /// Default release provider (e.g., "registry", "task").
    #[serde(default)]
    pub default: Option<String>,
    /// Versioning scheme (e.g., "calver").
    #[serde(default)]
    pub versioning: Option<String>,
    /// Optional suffix for calver (appended as pre-release, e.g., "1" -> 2026.1.12-1).
    #[serde(default)]
    pub calver_suffix: Option<String>,
    /// Release host domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// Base URL for release artifacts.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Release host root path.
    #[serde(default)]
    pub root: Option<String>,
    /// Caddyfile path.
    #[serde(default)]
    pub caddyfile: Option<String>,
    /// Readme file path to update.
    #[serde(default)]
    pub readme: Option<String>,
    /// Flow registry release config.
    #[serde(default)]
    pub registry: Option<RegistryReleaseConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RegistryReleaseConfig {
    /// Base URL for the registry (e.g., "https://myflow.sh").
    #[serde(default)]
    pub url: Option<String>,
    /// Registry package name (defaults to project name).
    #[serde(default)]
    pub package: Option<String>,
    /// Optional binary names to upload.
    #[serde(default)]
    pub bins: Option<Vec<String>>,
    /// Default binary name to install.
    #[serde(default)]
    pub default_bin: Option<String>,
    /// Env var that holds the registry token.
    #[serde(default)]
    pub token_env: Option<String>,
    /// Whether to update the latest pointer by default.
    #[serde(default)]
    pub latest: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SetupConfig {
    /// Server setup defaults (used by f setup release).
    #[serde(default)]
    pub server: Option<SetupServerConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SetupServerConfig {
    /// Optional template flow.toml path to pull [host] defaults from.
    pub template: Option<String>,
    /// Optional inline [host] defaults.
    #[serde(default)]
    pub host: Option<crate::deploy::HostConfig>,
}

/// Global feature toggles.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OptionsConfig {
    #[serde(default, rename = "trace_terminal_io")]
    pub trace_terminal_io: bool,
    #[serde(
        default,
        rename = "commit_with_check_async",
        alias = "commit-with-check-async"
    )]
    pub commit_with_check_async: Option<bool>,
    #[serde(
        default,
        rename = "commit_with_check_use_repo_root",
        alias = "commit-with-check-use-repo-root"
    )]
    pub commit_with_check_use_repo_root: Option<bool>,
    #[serde(
        default,
        rename = "commit_with_check_timeout_secs",
        alias = "commit-with-check-timeout-secs"
    )]
    pub commit_with_check_timeout_secs: Option<u64>,
    /// Number of retries when review times out (default 1).
    #[serde(
        default,
        rename = "commit_with_check_review_retries",
        alias = "commit-with-check-review-retries"
    )]
    pub commit_with_check_review_retries: Option<u32>,
    /// Remote Claude review URL for commitWithCheck.
    #[serde(
        default,
        rename = "commit_with_check_review_url",
        alias = "commit-with-check-review-url"
    )]
    pub commit_with_check_review_url: Option<String>,
    /// Optional auth token for remote review.
    #[serde(
        default,
        rename = "commit_with_check_review_token",
        alias = "commit-with-check-review-token"
    )]
    pub commit_with_check_review_token: Option<String>,
    /// Enable mirroring commits to gitedit.dev for commitWithCheck.
    #[serde(
        default,
        rename = "commit_with_check_gitedit_mirror",
        alias = "commit-with-check-gitedit-mirror"
    )]
    pub commit_with_check_gitedit_mirror: Option<bool>,
    /// Enable mirroring commits to gitedit.dev (opt-in per project).
    #[serde(default, rename = "gitedit_mirror", alias = "gitedit-mirror")]
    pub gitedit_mirror: Option<bool>,
    /// Custom gitedit API URL (defaults to https://gitedit.dev).
    #[serde(default, rename = "gitedit_url", alias = "gitedit-url")]
    pub gitedit_url: Option<String>,
    /// Override repo full name for gitedit sync (e.g., "giteditdev/gitedit").
    #[serde(
        default,
        rename = "gitedit_repo_full_name",
        alias = "gitedit-repo-full-name"
    )]
    pub gitedit_repo_full_name: Option<String>,
    /// Optional token for gitedit sync/publish.
    #[serde(default, rename = "gitedit_token", alias = "gitedit-token")]
    pub gitedit_token: Option<String>,
    /// Enable mirroring commits to myflow.sh (opt-in per project).
    #[serde(default, rename = "myflow_mirror", alias = "myflow-mirror")]
    pub myflow_mirror: Option<bool>,
    /// Custom myflow API URL (defaults to https://myflow.sh).
    #[serde(default, rename = "myflow_url", alias = "myflow-url")]
    pub myflow_url: Option<String>,
    /// Optional token for myflow sync.
    #[serde(default, rename = "myflow_token", alias = "myflow-token")]
    pub myflow_token: Option<String>,
}

impl OptionsConfig {
    fn merge(&mut self, other: OptionsConfig) {
        if other.trace_terminal_io {
            self.trace_terminal_io = true;
        }
        if other.commit_with_check_async.is_some() {
            self.commit_with_check_async = other.commit_with_check_async;
        }
        if other.commit_with_check_use_repo_root.is_some() {
            self.commit_with_check_use_repo_root = other.commit_with_check_use_repo_root;
        }
        if other.commit_with_check_timeout_secs.is_some() {
            self.commit_with_check_timeout_secs = other.commit_with_check_timeout_secs;
        }
        if other.commit_with_check_review_retries.is_some() {
            self.commit_with_check_review_retries = other.commit_with_check_review_retries;
        }
        if other.commit_with_check_review_url.is_some() {
            self.commit_with_check_review_url = other.commit_with_check_review_url;
        }
        if other.commit_with_check_review_token.is_some() {
            self.commit_with_check_review_token = other.commit_with_check_review_token;
        }
        if other.commit_with_check_gitedit_mirror.is_some() {
            self.commit_with_check_gitedit_mirror = other.commit_with_check_gitedit_mirror;
        }
        if other.gitedit_mirror.is_some() {
            self.gitedit_mirror = other.gitedit_mirror;
        }
        if other.gitedit_url.is_some() {
            self.gitedit_url = other.gitedit_url;
        }
        if other.gitedit_repo_full_name.is_some() {
            self.gitedit_repo_full_name = other.gitedit_repo_full_name;
        }
        if other.gitedit_token.is_some() {
            self.gitedit_token = other.gitedit_token;
        }
        if other.myflow_mirror.is_some() {
            self.myflow_mirror = other.myflow_mirror;
        }
        if other.myflow_url.is_some() {
            self.myflow_url = other.myflow_url;
        }
        if other.myflow_token.is_some() {
            self.myflow_token = other.myflow_token;
        }
    }
}

/// Configuration for a single managed HTTP server process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Human-friendly name used in the TUI and HTTP API.
    pub name: String,
    /// Program to execute, e.g. "node", "cargo".
    pub command: String,
    /// Arguments passed to the command.
    pub args: Vec<String>,
    /// Optional port the server listens on (for display only).
    pub port: Option<u16>,
    /// Optional working directory for the process.
    pub working_dir: Option<PathBuf>,
    /// Additional environment variables.
    pub env: HashMap<String, String>,
    /// Whether this server should be started automatically with the daemon.
    pub autostart: bool,
}

impl ServerConfig {
    pub fn to_daemon_config(&self) -> DaemonConfig {
        DaemonConfig {
            name: self.name.clone(),
            binary: self.command.clone(),
            command: None,
            args: self.args.clone(),
            working_dir: self
                .working_dir
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            port: self.port,
            env: self.env.clone(),
            autostart: self.autostart,
            restart: Some(DaemonRestartPolicy::OnFailure),
            description: Some(format!("Dev server: {}", self.name)),
            health_url: None,
            host: None,
            boot: false,
            autostop: false,
            retry: Some(3),
            ready_delay: None,
            ready_output: None,
        }
    }
}

impl<'de> Deserialize<'de> for ServerConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawServerConfig {
            #[serde(default)]
            name: Option<String>,
            command: String,
            #[serde(default)]
            args: Vec<String>,
            #[serde(default)]
            port: Option<u16>,
            #[serde(default, alias = "path")]
            working_dir: Option<String>,
            #[serde(default)]
            env: HashMap<String, String>,
            #[serde(default = "default_autostart")]
            autostart: bool,
        }

        let raw = RawServerConfig::deserialize(deserializer)?;
        let mut command = raw.command;
        let mut args = raw.args;

        if args.is_empty() {
            if let Ok(parts) = shell_words::split(&command) {
                if let Some((head, tail)) = parts.split_first() {
                    command = head.clone();
                    args = tail.to_vec();
                }
            }
        }

        let name = raw
            .name
            .or_else(|| {
                raw.working_dir.as_ref().and_then(|dir| {
                    Path::new(dir)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .filter(|s| !s.is_empty())
                })
            })
            .unwrap_or_else(|| {
                if command.is_empty() {
                    "server".to_string()
                } else {
                    command.clone()
                }
            });

        let command = expand_path(&command).to_string_lossy().into_owned();

        Ok(ServerConfig {
            name,
            command,
            args,
            port: raw.port,
            working_dir: raw.working_dir.map(|dir| expand_path(&dir)),
            env: raw.env,
            autostart: raw.autostart,
        })
    }
}

fn default_autostart() -> bool {
    true
}

/// Local project automation task description.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskConfig {
    /// Unique identifier for the task (used when selecting it interactively).
    pub name: String,
    /// Shell command that should be executed for this task.
    pub command: String,
    /// Whether this task should be handed off to the hub daemon instead of running locally.
    #[serde(default, rename = "delegate-to-hub", alias = "delegate_to_hub")]
    pub delegate_to_hub: bool,
    /// Whether this task should run automatically when entering the project root.
    #[serde(default)]
    pub activate_on_cd_to_root: bool,
    /// Optional task-specific dependencies that must be made available before the command runs.
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Optional human-friendly description.
    #[serde(default, alias = "desc")]
    pub description: Option<String>,
    /// Optional short aliases that `f run` should recognize (e.g. "dcr" for "deploy-cli-release").
    #[serde(
        default,
        alias = "shortcut",
        alias = "short",
        deserialize_with = "deserialize_shortcuts"
    )]
    pub shortcuts: Vec<String>,
    /// Whether this task requires interactive input (stdin passthrough, TTY).
    #[serde(default)]
    pub interactive: bool,
    /// Require confirmation when matched via LM Studio (for destructive tasks).
    #[serde(default, alias = "confirm-on-match")]
    pub confirm_on_match: bool,
    /// Command to run when the task is cancelled (Ctrl+C).
    #[serde(default, alias = "on-cancel")]
    pub on_cancel: Option<String>,
    /// Optional file path to save combined task output (relative to project root unless absolute).
    #[serde(default, alias = "output-file")]
    pub output_file: Option<String>,
}

/// Definition of a dependency that can be referenced by automation tasks.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DependencySpec {
    /// Single command/binary that should be available on PATH.
    Single(String),
    /// Multiple commands that should be checked together.
    Multiple(Vec<String>),
    /// Flox package descriptor that should be added to the local env manifest.
    Flox(FloxInstallSpec),
}

fn deserialize_shortcuts<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ShortcutField {
        Single(String),
        Multiple(Vec<String>),
    }

    let value = Option::<ShortcutField>::deserialize(deserializer)?;
    let shortcuts = match value {
        Some(ShortcutField::Single(alias)) => vec![alias],
        Some(ShortcutField::Multiple(aliases)) => aliases,
        None => Vec::new(),
    };
    Ok(shortcuts)
}

/// Storage configuration describing remote environments providers.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    /// Provider identifier understood by the hosted hub.
    pub provider: String,
    /// Environment variable that stores the API key/token.
    #[serde(default = "default_storage_env_var")]
    pub env_var: String,
    /// Base URL for the storage hub (defaults to hosted flow hub).
    #[serde(default = "default_hub_url")]
    pub hub_url: String,
    /// Environments that can be synced locally.
    #[serde(default)]
    pub envs: Vec<StorageEnvConfig>,
}

fn default_hub_url() -> String {
    "https://myflow.sh".to_string()
}

fn default_storage_env_var() -> String {
    "FLOW_SECRETS_TOKEN".to_string()
}

/// Definition of an environment with named variables.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageEnvConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub variables: Vec<StorageVariable>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageVariable {
    pub key: String,
    #[serde(default)]
    pub default: Option<String>,
}

/// Flox manifest-style configuration (install set, etc.).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FloxConfig {
    #[serde(default, rename = "install", alias = "deps")]
    pub install: HashMap<String, FloxInstallSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FloxInstallSpec {
    #[serde(rename = "pkg-path")]
    pub pkg_path: String,
    #[serde(default, rename = "pkg-group")]
    pub pkg_group: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub systems: Option<Vec<String>>,
    #[serde(default)]
    pub priority: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandFileConfig {
    pub path: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteServerConfig {
    #[serde(flatten)]
    pub server: ServerConfig,
    /// Optional hub name that coordinates this remote process.
    #[serde(default)]
    pub hub: Option<String>,
    /// Paths to sync to the remote hub before launching.
    #[serde(default)]
    pub sync_paths: Vec<PathBuf>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ServerHubConfig {
    pub name: String,
    pub host: String,
    #[serde(default = "default_server_hub_port")]
    pub port: u16,
    #[serde(default)]
    pub tailscale: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

fn default_server_hub_port() -> u16 {
    9050
}

/// File watcher configuration for local automation.
#[derive(Debug, Clone, Deserialize)]
pub struct WatcherConfig {
    #[serde(default)]
    pub driver: WatcherDriver,
    pub name: String,
    pub path: String,
    #[serde(default, rename = "match")]
    pub filter: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default)]
    pub run_on_start: bool,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub poltergeist: Option<PoltergeistConfig>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WatcherDriver {
    Shell,
    Poltergeist,
}

impl Default for WatcherDriver {
    fn default() -> Self {
        WatcherDriver::Shell
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoltergeistConfig {
    #[serde(default = "default_poltergeist_binary")]
    pub binary: String,
    #[serde(default)]
    pub mode: PoltergeistMode,
    #[serde(default)]
    pub args: Vec<String>,
}

impl Default for PoltergeistConfig {
    fn default() -> Self {
        Self {
            binary: default_poltergeist_binary(),
            mode: PoltergeistMode::Haunt,
            args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoltergeistMode {
    Haunt,
    Panel,
    Status,
}

impl Default for PoltergeistMode {
    fn default() -> Self {
        PoltergeistMode::Haunt
    }
}

fn default_debounce_ms() -> u64 {
    200
}

fn default_poltergeist_binary() -> String {
    "poltergeist".to_string()
}

impl PoltergeistMode {
    pub fn as_subcommand(&self) -> &'static str {
        match self {
            PoltergeistMode::Haunt => "haunt",
            PoltergeistMode::Panel => "panel",
            PoltergeistMode::Status => "status",
        }
    }
}

/// Streaming configuration handled by the hub (stub for future OBS integration).
#[derive(Debug, Clone, Deserialize)]
pub struct StreamConfig {
    pub provider: String,
    #[serde(default)]
    pub hotkey: Option<String>,
    #[serde(default)]
    pub toggle_url: Option<String>,
}

/// Restart behavior for managed daemons.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DaemonRestartPolicy {
    Never,
    OnFailure,
    Always,
}

/// Configuration for a background daemon that flow can manage.
///
/// Example in flow.toml:
/// ```toml
/// [[daemon]]
/// name = "lin"
/// binary = "lin"
/// command = "daemon"
/// args = ["--host", "127.0.0.1", "--port", "9050"]
/// health_url = "http://127.0.0.1:9050/health"
///
/// [[daemon]]
/// name = "base"
/// binary = "base"
/// command = "jazz"
/// args = ["--port", "7201"]
/// health_url = "http://127.0.0.1:7201/health"
/// working_dir = "~/code/myflow"
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct DaemonConfig {
    /// Unique name for this daemon (used in `f daemon start <name>`).
    pub name: String,
    /// Binary to execute (can be a name on PATH or absolute path).
    pub binary: String,
    /// Subcommand to run the daemon (e.g., "daemon", "jazz", "serve").
    #[serde(default)]
    pub command: Option<String>,
    /// Additional arguments passed after the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Health check URL to determine if daemon is running.
    #[serde(default, alias = "health")]
    pub health_url: Option<String>,
    /// Port the daemon listens on (extracted from health_url if not specified).
    #[serde(default)]
    pub port: Option<u16>,
    /// Host the daemon binds to.
    #[serde(default)]
    pub host: Option<String>,
    /// Working directory for the daemon process.
    #[serde(default, alias = "path")]
    pub working_dir: Option<String>,
    /// Environment variables to set for the daemon.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Whether to start this daemon automatically when flow starts.
    #[serde(default)]
    pub autostart: bool,
    /// Whether to stop this daemon when leaving the project.
    #[serde(default)]
    pub autostop: bool,
    /// Whether to start this daemon during boot/startup sessions.
    #[serde(default)]
    pub boot: bool,
    /// Restart policy (never, on-failure, always).
    #[serde(default)]
    pub restart: Option<DaemonRestartPolicy>,
    /// Maximum restart attempts (optional).
    #[serde(default)]
    pub retry: Option<u32>,
    /// Milliseconds to wait before considering the daemon ready.
    #[serde(default)]
    pub ready_delay: Option<u64>,
    /// Output pattern (string or regex) to match for readiness.
    #[serde(default)]
    pub ready_output: Option<String>,
    /// Description of what this daemon does.
    #[serde(default)]
    pub description: Option<String>,
}

impl DaemonConfig {
    /// Get the effective health URL, building from host/port if not specified.
    pub fn effective_health_url(&self) -> Option<String> {
        if let Some(url) = &self.health_url {
            return Some(url.clone());
        }
        let host = self.host.as_deref().unwrap_or("127.0.0.1");
        self.port.map(|p| format!("http://{}:{}/health", host, p))
    }

    /// Get the effective host.
    pub fn effective_host(&self) -> &str {
        self.host.as_deref().unwrap_or("127.0.0.1")
    }
}

impl DependencySpec {
    /// Add one or more command names to the provided buffer.
    pub fn extend_commands(&self, buffer: &mut Vec<String>) {
        match self {
            DependencySpec::Single(cmd) => buffer.push(cmd.clone()),
            DependencySpec::Multiple(cmds) => buffer.extend(cmds.iter().cloned()),
            DependencySpec::Flox(_) => {}
        }
    }
}

fn deserialize_aliases<'de, D>(deserializer: D) -> Result<HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum AliasInput {
        Map(HashMap<String, String>),
        List(Vec<HashMap<String, String>>),
    }

    let maybe = Option::<AliasInput>::deserialize(deserializer)?;
    let mut aliases = HashMap::new();
    if let Some(input) = maybe {
        match input {
            AliasInput::Map(map) => aliases = map,
            AliasInput::List(list) => {
                for table in list {
                    for (name, command) in table {
                        aliases.insert(name, command);
                    }
                }
            }
        }
    }

    Ok(aliases)
}

/// Default config path: ~/.config/flow/flow.toml (falls back to legacy config.toml)
pub fn default_config_path() -> PathBuf {
    let base = global_config_dir();

    let primary = base.join("flow.toml");
    if primary.exists() {
        return primary;
    }

    let legacy = base.join("config.toml");
    if legacy.exists() {
        tracing::warn!("using legacy config path ~/.config/flow/config.toml; rename to flow.toml");
        return legacy;
    }

    primary
}

/// Global config directory: ~/.config/flow
pub fn global_config_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/flow")
}

/// Ensure the global config directory exists (moves aside files that block it).
pub fn ensure_global_config_dir() -> Result<PathBuf> {
    let dir = global_config_dir();
    if let Some(parent) = dir.parent() {
        ensure_dir(parent)?;
    }
    ensure_dir(&dir)?;
    Ok(dir)
}

/// Global state directory for runtime data.
pub fn global_state_dir() -> PathBuf {
    let config_dir = global_config_dir();
    if is_dir_path(&config_dir) {
        return config_dir;
    }

    config_dir.with_file_name("flow-state")
}

/// Ensure the global state directory exists.
pub fn ensure_global_state_dir() -> Result<PathBuf> {
    let dir = global_state_dir();
    if let Some(parent) = dir.parent() {
        ensure_dir(parent)?;
    }
    ensure_dir(&dir)?;
    Ok(dir)
}

fn ensure_dir(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        let is_dir = meta.is_dir();
        let is_symlink = meta.file_type().is_symlink();
        if is_dir {
            return Ok(());
        }
        if is_symlink {
            if let Ok(target_meta) = fs::metadata(path) {
                if target_meta.is_dir() {
                    return Ok(());
                }
            }
        }

        let backup = backup_path(path);
        fs::rename(path, &backup).with_context(|| {
            format!(
                "failed to move existing {} to {}",
                path.display(),
                backup.display()
            )
        })?;
        tracing::warn!(
            "moved blocking path {} to {}",
            path.display(),
            backup.display()
        );
    }

    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(())
}

fn is_dir_path(path: &Path) -> bool {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.is_dir() {
            return true;
        }
        if meta.file_type().is_symlink() {
            if let Ok(target_meta) = fs::metadata(path) {
                return target_meta.is_dir();
            }
        }
    }
    false
}

fn backup_path(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("flow");
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    path.with_file_name(format!("{}-archive-{}", name, ts))
}

/// Load global secrets from ~/.config/flow/secrets.toml
pub fn load_global_secrets() {
    let secrets_path = global_config_dir().join("secrets.toml");
    if secrets_path.exists() {
        if let Ok(secrets) = load_secrets(&secrets_path) {
            let mut dummy = Config::default();
            merge_secrets(&mut dummy, secrets);
            tracing::debug!(path = %secrets_path.display(), "loaded global secrets");
        }
    }
}

/// Path to TypeScript config: ~/.config/flow/config.ts
pub fn ts_config_path() -> PathBuf {
    global_config_dir().join("config.ts")
}

/// Load TypeScript config from ~/.config/flow/config.ts using bun.
/// Returns None if config.ts doesn't exist or fails to load.
pub fn load_ts_config() -> Option<TsConfig> {
    let config_path = ts_config_path();
    if !config_path.exists() {
        return None;
    }

    // Use bun to evaluate the TypeScript and output JSON
    let loader_script = format!(
        r#"const config = await import("{}"); console.log(JSON.stringify(config.default || config));"#,
        config_path.display()
    );

    let mut child = std::process::Command::new("bun")
        .args(["run", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(loader_script.as_bytes());
    }

    let output = child.wait_with_output().ok()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("failed to load config.ts: {}", stderr.trim());
        return None;
    }

    let json = String::from_utf8_lossy(&output.stdout);
    match serde_json::from_str::<TsConfig>(json.trim()) {
        Ok(config) => {
            tracing::debug!(path = %config_path.display(), "loaded TypeScript config");
            Some(config)
        }
        Err(err) => {
            tracing::warn!("failed to parse config.ts output: {}", err);
            None
        }
    }
}

/// Preferred env backend from ~/.config/flow/config.ts ("cloud" or "local").
pub fn preferred_env_backend() -> Option<String> {
    let config = load_ts_config()?;
    let backend = config.flow?.env?.backend?;
    let trimmed = backend.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_ascii_lowercase())
}

/// Env vars to inject into every task from the personal env store.
/// Defaults to AI server connection vars unless overridden in config.ts.
pub fn global_env_keys() -> Vec<String> {
    static GLOBAL_KEYS: OnceLock<Vec<String>> = OnceLock::new();
    GLOBAL_KEYS
        .get_or_init(|| {
            let mut keys = vec![
                "AI_SERVER_URL".to_string(),
                "AI_SERVER_TOKEN".to_string(),
                "AI_SERVER_MODEL".to_string(),
                "ZAI_API_KEY".to_string(),
            ];

            if let Some(config) = load_ts_config() {
                if let Some(env) = config.flow.and_then(|flow| flow.env) {
                    if !env.global_keys.is_empty() {
                        keys = env.global_keys;
                    }
                }
            }

            keys
        })
        .clone()
}

pub fn expand_path(raw: &str) -> PathBuf {
    let tilde_expanded = tilde(raw).into_owned();
    let env_expanded = match shellexpand::env(&tilde_expanded) {
        Ok(val) => val.into_owned(),
        Err(_) => tilde_expanded,
    };
    PathBuf::from(env_expanded)
}

pub fn load<P: AsRef<Path>>(path: P) -> Result<Config> {
    let path = path.as_ref();
    let mut visited = Vec::new();
    let mut cfg = load_with_includes(path, &mut visited)?;

    // Load secrets from secrets.toml in the same directory (never shown on stream)
    if let Some(parent) = path.parent() {
        let secrets_path = parent.join("secrets.toml");
        if secrets_path.exists() {
            if let Ok(secrets) = load_secrets(&secrets_path) {
                merge_secrets(&mut cfg, secrets);
                tracing::debug!(path = %secrets_path.display(), "loaded secrets file");
            }
        }
    }

    Ok(cfg)
}

/// Secrets that can be loaded from a separate file to avoid exposing on stream.
#[derive(Debug, Clone, Default, Deserialize)]
struct Secrets {
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    cloudflare: Option<CloudflareSecrets>,
    #[serde(default)]
    openai: Option<ApiKeySecret>,
    #[serde(default)]
    anthropic: Option<ApiKeySecret>,
    #[serde(default)]
    cerebras: Option<ApiKeySecret>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CloudflareSecrets {
    account_id: Option<String>,
    stream_token: Option<String>,
    stream_key: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ApiKeySecret {
    #[serde(alias = "api_key", alias = "key")]
    api_key: Option<String>,
}

fn load_secrets(path: &Path) -> Result<Secrets> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read secrets at {}", path.display()))?;
    let secrets: Secrets = toml::from_str(&contents)
        .with_context(|| format!("failed to parse secrets at {}", path.display()))?;
    Ok(secrets)
}

fn merge_secrets(cfg: &mut Config, secrets: Secrets) {
    // Inject secrets as environment variables for child processes
    // SAFETY: We're setting env vars at startup before any threads are spawned
    unsafe {
        for (key, value) in secrets.env {
            std::env::set_var(&key, &value);
        }
        if let Some(cf) = secrets.cloudflare {
            if let Some(v) = cf.account_id {
                std::env::set_var("CLOUDFLARE_ACCOUNT_ID", &v);
            }
            if let Some(v) = cf.stream_token {
                std::env::set_var("CLOUDFLARE_STREAM_TOKEN", &v);
            }
            if let Some(v) = cf.stream_key {
                std::env::set_var("CLOUDFLARE_STREAM_KEY", &v);
            }
        }
        if let Some(openai) = secrets.openai {
            if let Some(v) = openai.api_key {
                std::env::set_var("OPENAI_API_KEY", &v);
            }
        }
        if let Some(anthropic) = secrets.anthropic {
            if let Some(v) = anthropic.api_key {
                std::env::set_var("ANTHROPIC_API_KEY", &v);
            }
        }
        if let Some(cerebras) = secrets.cerebras {
            if let Some(v) = cerebras.api_key {
                std::env::set_var("CEREBRAS_API_KEY", &v);
            }
        }
    }
    // Storage config can also reference these env vars
    let _ = cfg; // cfg itself doesn't need modification, env vars are set
}

fn load_with_includes(path: &Path, visited: &mut Vec<PathBuf>) -> Result<Config> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve path {}", path.display()))?;
    if visited.contains(&canonical) {
        anyhow::bail!(
            "cycle detected while loading config includes: {}",
            path.display()
        );
    }
    visited.push(canonical.clone());

    let contents = fs::read_to_string(&canonical)
        .with_context(|| format!("failed to read flow config at {}", path.display()))?;
    let mut cfg: Config = match toml::from_str(&contents) {
        Ok(cfg) => cfg,
        Err(err) => {
            let fix = fixup::fix_toml_content(&contents);
            if fix.fixes_applied.is_empty() {
                return Err(err)
                    .with_context(|| format!("failed to parse flow config at {}", path.display()));
            }
            let fixed = fixup::apply_fixes_to_content(&contents, &fix.fixes_applied);
            if let Err(write_err) = fs::write(&canonical, &fixed) {
                return Err(err)
                    .with_context(|| format!("failed to parse flow config at {}", path.display()))
                    .with_context(|| format!("auto-fix write failed: {}", write_err));
            }
            toml::from_str(&fixed).with_context(|| {
                format!(
                    "failed to parse flow config at {} (after auto-fix)",
                    path.display()
                )
            })?
        }
    };

    for include in cfg.command_files.clone() {
        let include_path = resolve_include_path(&canonical, &include.path);
        if let Some(description) = include.description.as_deref() {
            tracing::debug!(
                path = %include_path.display(),
                description,
                "loading additional command file"
            );
        }
        let included = load_with_includes(&include_path, visited)
            .with_context(|| format!("failed to load commands file {}", include_path.display()))?;
        merge_config(&mut cfg, included);
    }

    visited.pop();
    Ok(cfg)
}

fn resolve_include_path(base: &Path, include: &str) -> PathBuf {
    let include_path = PathBuf::from(include);
    if include_path.is_absolute() {
        include_path
    } else if let Some(parent) = base.parent() {
        parent.join(include_path)
    } else {
        include_path
    }
}

fn merge_config(base: &mut Config, other: Config) {
    if base.project_name.is_none() {
        base.project_name = other.project_name;
    }
    if base.flow.primary_task.is_none() {
        base.flow.primary_task = other.flow.primary_task;
    }
    if base.flow.release_task.is_none() {
        base.flow.release_task = other.flow.release_task;
    }
    if base.flow.deploy_task.is_none() {
        base.flow.deploy_task = other.flow.deploy_task;
    }
    merge_release_config(base, other.release);
    if base.setup.is_none() {
        base.setup = other.setup;
    } else if let (Some(base_setup), Some(other_setup)) = (base.setup.as_mut(), other.setup) {
        if base_setup.server.is_none() {
            base_setup.server = other_setup.server;
        } else if let (Some(base_server), Some(other_server)) =
            (base_setup.server.as_mut(), other_setup.server)
        {
            if base_server.template.is_none() {
                base_server.template = other_server.template;
            }
            if base_server.host.is_none() {
                base_server.host = other_server.host;
            }
        }
    }
    if base.analytics.is_none() {
        base.analytics = other.analytics;
    }
    if base.jj.is_none() {
        base.jj = other.jj;
    }
    base.options.merge(other.options);
    base.servers.extend(other.servers);
    base.remote_servers.extend(other.remote_servers);
    base.tasks.extend(other.tasks);
    base.watchers.extend(other.watchers);
    base.daemons.extend(other.daemons);
    base.stream = base.stream.take().or(other.stream);
    base.storage = base.storage.take().or(other.storage);
    base.server_hub = base.server_hub.take().or(other.server_hub);
    for (key, value) in other.aliases {
        base.aliases.entry(key).or_insert(value);
    }
    for (key, value) in other.dependencies {
        base.dependencies.entry(key).or_insert(value);
    }
    match (&mut base.flox, other.flox) {
        (Some(base_flox), Some(other_flox)) => {
            for (key, value) in other_flox.install {
                base_flox.install.entry(key).or_insert(value);
            }
        }
        (None, Some(other_flox)) => base.flox = Some(other_flox),
        _ => {}
    }
}

fn merge_release_config(base: &mut Config, other: Option<ReleaseConfig>) {
    let Some(other) = other else {
        return;
    };
    let base_release = base.release.get_or_insert_with(ReleaseConfig::default);

    if base_release.default.is_none() {
        base_release.default = other.default;
    }
    if base_release.domain.is_none() {
        base_release.domain = other.domain;
    }
    if base_release.base_url.is_none() {
        base_release.base_url = other.base_url;
    }
    if base_release.root.is_none() {
        base_release.root = other.root;
    }
    if base_release.caddyfile.is_none() {
        base_release.caddyfile = other.caddyfile;
    }
    if base_release.readme.is_none() {
        base_release.readme = other.readme;
    }

    if let Some(other_registry) = other.registry {
        let registry = base_release
            .registry
            .get_or_insert_with(RegistryReleaseConfig::default);
        if registry.url.is_none() {
            registry.url = other_registry.url;
        }
        if registry.package.is_none() {
            registry.package = other_registry.package;
        }
        if registry.bins.is_none() {
            registry.bins = other_registry.bins;
        }
        if registry.default_bin.is_none() {
            registry.default_bin = other_registry.default_bin;
        }
        if registry.token_env.is_none() {
            registry.token_env = other_registry.token_env;
        }
        if registry.latest.is_none() {
            registry.latest = other_registry.latest;
        }
    }
}

/// Load config from the given path, logging a warning and returning an empty
/// config if anything goes wrong. This keeps the daemon usable even if the
/// config file is missing or invalid.
pub fn load_or_default<P: AsRef<Path>>(path: P) -> Config {
    match load(path) {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::warn!(
                ?err,
                "failed to load flow config; starting with no managed servers"
            );
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_path(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
    }

    #[test]
    fn load_parses_global_fixture() {
        let cfg = load(fixture_path("test-data/global-config/flow.toml"))
            .expect("global config fixture should parse");

        assert_eq!(cfg.version, Some(1));
        assert!(cfg.options.trace_terminal_io, "options table should parse");
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.remote_servers.len(), 1);
        assert_eq!(cfg.watchers.len(), 1);
        assert_eq!(
            cfg.tasks.len(),
            1,
            "global config should inherit tasks from included command files"
        );

        let watcher = &cfg.watchers[0];
        assert_eq!(watcher.driver, WatcherDriver::Shell);
        assert_eq!(watcher.name, "karabiner");
        assert_eq!(watcher.path, "~/config/i/karabiner");
        assert_eq!(watcher.filter.as_deref(), Some("karabiner.edn"));
        assert_eq!(watcher.command.as_deref(), Some("~/bin/goku"));
        assert_eq!(watcher.debounce_ms, 150);
        assert!(watcher.run_on_start);
        assert!(watcher.poltergeist.is_none());

        let server = &cfg.servers[0];
        assert_eq!(server.name, "cloud");
        assert_eq!(server.command, "blade");
        assert_eq!(server.args, ["--port", "4000"]);
        let working_dir = server
            .working_dir
            .as_ref()
            .expect("server working dir should parse");
        assert!(
            working_dir.ends_with("code/myflow"),
            "unexpected working dir: {}",
            working_dir.display()
        );
        assert!(server.env.is_empty());
        assert!(
            server.autostart,
            "autostart should default to true when omitted"
        );

        let sync_task = &cfg.tasks[0];
        assert_eq!(sync_task.name, "sync-config");
        assert_eq!(
            sync_task.command,
            "rsync -av ~/.config/flow remote:~/flow-config"
        );
        assert!(
            cfg.aliases.contains_key("fsh"),
            "included aliases should merge into base config"
        );

        let remote = &cfg.remote_servers[0];
        assert_eq!(remote.server.name, "homelab-blade");
        assert_eq!(remote.hub.as_deref(), Some("homelab"));
        assert_eq!(remote.sync_paths, [PathBuf::from("~/config/i/karabiner")]);

        let hub = cfg.server_hub.as_ref().expect("server hub config");
        assert_eq!(hub.name, "homelab");
        assert_eq!(hub.host, "tailscale");
        assert_eq!(hub.port, 9050);
        assert_eq!(hub.tailscale.as_deref(), Some("linux-hub"));
    }

    #[test]
    fn server_port_is_preserved_when_present() {
        let toml = r#"
            [[server]]
            name = "api"
            command = "npm start"
            port = 8080
        "#;

        let cfg: Config = toml::from_str(toml).expect("server config should parse");
        let server = cfg.servers.first().expect("server should parse");
        assert_eq!(server.port, Some(8080));

        // Missing port should deserialize as None for backward compatibility.
        let no_port_toml = r#"
            [[server]]
            name = "web"
            command = "npm run dev"
        "#;
        let cfg: Config =
            toml::from_str(no_port_toml).expect("server config without port should parse");
        assert_eq!(cfg.servers[0].port, None);
    }

    #[test]
    fn expand_path_supports_tilde_and_env() {
        let home = std::env::var("HOME").expect("HOME must be set for tests");
        let expected = PathBuf::from(&home).join("projects/demo");

        assert_eq!(expand_path("~/projects/demo"), expected);
        assert_eq!(expand_path("$HOME/projects/demo"), expected);
    }

    #[test]
    fn parses_poltergeist_watcher() {
        let toml = r#"
            [[watchers]]
            driver = "poltergeist"
            name = "peekaboo"
            path = "~/code/myflow/peekaboo"

            [watchers.poltergeist]
            binary = "/opt/bin/poltergeist"
            mode = "panel"
            args = ["status", "--verbose"]
        "#;

        let cfg: Config = toml::from_str(toml).expect("poltergeist watcher should parse");
        assert_eq!(cfg.watchers.len(), 1);
        let watcher = &cfg.watchers[0];
        assert_eq!(watcher.driver, WatcherDriver::Poltergeist);
        assert_eq!(watcher.command, None);
        assert_eq!(watcher.path, "~/code/myflow/peekaboo");

        let poltergeist = watcher
            .poltergeist
            .as_ref()
            .expect("poltergeist config should exist");
        assert_eq!(poltergeist.binary, "/opt/bin/poltergeist");
        assert_eq!(poltergeist.mode, PoltergeistMode::Panel);
        assert_eq!(poltergeist.args, vec!["status", "--verbose"]);
    }

    #[test]
    fn load_or_default_returns_empty_when_missing() {
        let missing_path = fixture_path("test-data/global-config/does-not-exist.toml");
        let cfg = load_or_default(missing_path);
        assert!(
            cfg.servers.is_empty(),
            "missing config should fall back to empty server list"
        );
    }

    #[test]
    fn load_parses_project_tasks() {
        let cfg = load(fixture_path("test-data/simple-project/flow.toml"))
            .expect("simple project config should parse");

        assert!(cfg.servers.is_empty(), "project fixture focuses on tasks");
        assert_eq!(cfg.tasks.len(), 2);

        let lint = &cfg.tasks[0];
        assert_eq!(lint.name, "lint");
        assert_eq!(lint.command, "golangci-lint run");
        assert_eq!(
            lint.description.as_deref(),
            Some("Run static analysis for Go sources")
        );

        let test_task = &cfg.tasks[1];
        assert_eq!(test_task.name, "test");
        assert_eq!(test_task.command, "gotestsum -f pkgname ./...");
        assert_eq!(
            test_task.description.as_deref(),
            Some("Execute the Go test suite with gotestsum output"),
            "desc alias should populate description"
        );
    }

    #[test]
    fn load_parses_dependency_table() {
        let contents = r#"
[dependencies]
fast = "fast"
toolkit = ["rg", "fd"]

[[tasks]]
name = "ci"
command = "ci"
dependencies = ["fast", "toolkit"]
"#;
        let cfg: Config =
            toml::from_str(contents).expect("inline config with dependencies should parse");

        let task = cfg.tasks.first().expect("task should parse");
        assert_eq!(
            task.dependencies,
            ["fast", "toolkit"],
            "task dependency references should parse"
        );

        let fast = cfg
            .dependencies
            .get("fast")
            .expect("fast dependency should be present");
        match fast {
            DependencySpec::Single(expr) => {
                assert_eq!(expr, "fast");
            }
            other => panic!("fast dependency variant mismatch: {other:?}"),
        }

        let toolkit = cfg
            .dependencies
            .get("toolkit")
            .expect("toolkit dependency should be present");
        match toolkit {
            DependencySpec::Multiple(exprs) => {
                assert_eq!(exprs, &["rg", "fd"]);
            }
            other => panic!("toolkit dependency variant mismatch: {other:?}"),
        }
    }

    #[test]
    fn parses_flox_dependencies_and_config() {
        let contents = r#"
[dependencies]
rg.pkg-path = "ripgrep"

[flox.deps]
fd.pkg-path = "fd"
"#;

        let cfg: Config = toml::from_str(contents).expect("config with flox deps should parse");

        match cfg.dependencies.get("rg") {
            Some(DependencySpec::Flox(spec)) => {
                assert_eq!(spec.pkg_path, "ripgrep");
            }
            other => panic!("unexpected dependency variant: {other:?}"),
        }

        let flox = cfg.flox.expect("flox config should exist");
        let fd = flox
            .install
            .get("fd")
            .expect("fd install should be present");
        assert_eq!(fd.pkg_path, "fd");
    }

    #[test]
    fn task_activation_flag_defaults_and_parses() {
        let toml = r#"
[[tasks]]
name = "lint"
command = "golangci-lint run"

[[tasks]]
name = "setup"
command = "cargo check"
activate_on_cd_to_root = true
"#;

        let cfg: Config = toml::from_str(toml).expect("activation config should parse");
        assert_eq!(cfg.tasks.len(), 2);
        assert!(!cfg.tasks[0].activate_on_cd_to_root);
        assert!(cfg.tasks[1].activate_on_cd_to_root);
    }

    #[test]
    fn load_parses_aliases() {
        let contents = r#"
[aliases]
fr = "f run"
ls = "f tasks"
"#;
        let cfg: Config = toml::from_str(contents).expect("inline alias config should parse");
        assert_eq!(cfg.aliases.get("fr").map(String::as_str), Some("f run"));
        assert_eq!(cfg.aliases.get("ls").map(String::as_str), Some("f tasks"));
    }

    #[test]
    fn load_parses_alias_array_table() {
        let contents = r#"
[[alias]]
fr = "f run"
fc = "f commit"

[[alias]]
dev = "f run dev"
"#;
        let cfg: Config = toml::from_str(contents).expect("alias array config should parse");
        assert_eq!(cfg.aliases.get("fr").map(String::as_str), Some("f run"));
        assert_eq!(cfg.aliases.get("fc").map(String::as_str), Some("f commit"));
        assert_eq!(
            cfg.aliases.get("dev").map(String::as_str),
            Some("f run dev")
        );
    }

    #[test]
    fn options_defaults_are_false() {
        let cfg: Config =
            toml::from_str("").expect("empty config should parse with default options");
        assert!(!cfg.options.trace_terminal_io);
        assert!(cfg.options.commit_with_check_async.is_none());
        assert!(cfg.options.commit_with_check_use_repo_root.is_none());
        assert!(cfg.options.commit_with_check_timeout_secs.is_none());
        assert!(cfg.options.commit_with_check_gitedit_mirror.is_none());
    }

    #[test]
    fn options_trace_flag_parses() {
        let toml = r#"
[options]
trace_terminal_io = true
"#;
        let cfg: Config = toml::from_str(toml).expect("options table should parse");
        assert!(cfg.options.trace_terminal_io);
    }

    #[test]
    fn options_commit_with_check_timeout_parses() {
        let toml = r#"
[options]
commit_with_check_timeout_secs = 120
"#;
        let cfg: Config = toml::from_str(toml).expect("options table should parse");
        assert_eq!(cfg.options.commit_with_check_timeout_secs, Some(120));
    }

    #[test]
    fn options_commit_with_check_review_retries_parses() {
        let toml = r#"
[options]
commit_with_check_review_retries = 3
"#;
        let cfg: Config = toml::from_str(toml).expect("options table should parse");
        assert_eq!(cfg.options.commit_with_check_review_retries, Some(3));
    }

    #[test]
    fn options_commit_with_check_async_parses() {
        let toml = r#"
[options]
commit_with_check_async = false
"#;
        let cfg: Config = toml::from_str(toml).expect("options table should parse");
        assert_eq!(cfg.options.commit_with_check_async, Some(false));
    }

    #[test]
    fn options_commit_with_check_use_repo_root_parses() {
        let toml = r#"
[options]
commit_with_check_use_repo_root = false
"#;
        let cfg: Config = toml::from_str(toml).expect("options table should parse");
        assert_eq!(cfg.options.commit_with_check_use_repo_root, Some(false));
    }

    #[test]
    fn options_commit_with_check_gitedit_mirror_parses() {
        let toml = r#"
[options]
commit_with_check_gitedit_mirror = true
"#;
        let cfg: Config = toml::from_str(toml).expect("options table should parse");
        assert_eq!(cfg.options.commit_with_check_gitedit_mirror, Some(true));
    }

    #[test]
    fn commit_testing_config_parses() {
        let toml = r#"
[commit.testing]
mode = "block"
runner = "bun"
bun_repo_strict = true
require_related_tests = true
ai_scratch_test_dir = ".ai/test"
run_ai_scratch_tests = true
allow_ai_scratch_to_satisfy_gate = false
max_local_gate_seconds = 20
"#;
        let cfg: Config = toml::from_str(toml).expect("commit.testing should parse");
        let commit = cfg.commit.expect("commit config expected");
        let testing = commit.testing.expect("testing config expected");
        assert_eq!(testing.mode.as_deref(), Some("block"));
        assert_eq!(testing.runner.as_deref(), Some("bun"));
        assert_eq!(testing.bun_repo_strict, Some(true));
        assert_eq!(testing.require_related_tests, Some(true));
        assert_eq!(testing.ai_scratch_test_dir.as_deref(), Some(".ai/test"));
        assert_eq!(testing.run_ai_scratch_tests, Some(true));
        assert_eq!(testing.allow_ai_scratch_to_satisfy_gate, Some(false));
        assert_eq!(testing.max_local_gate_seconds, Some(20));
    }

    #[test]
    fn commit_skill_gate_config_parses() {
        let toml = r#"
[commit.skill_gate]
mode = "block"
required = ["quality-bun-feature-delivery"]

[commit.skill_gate.min_version]
quality-bun-feature-delivery = 2
"#;
        let cfg: Config = toml::from_str(toml).expect("commit.skill_gate should parse");
        let commit = cfg.commit.expect("commit config expected");
        let skill_gate = commit.skill_gate.expect("skill gate config expected");
        assert_eq!(skill_gate.mode.as_deref(), Some("block"));
        assert_eq!(
            skill_gate.required,
            vec!["quality-bun-feature-delivery".to_string()]
        );
        let min_version = skill_gate.min_version.expect("min_version map expected");
        assert_eq!(min_version.get("quality-bun-feature-delivery"), Some(&2));
    }

    #[test]
    fn skills_codex_config_parses() {
        let toml = r#"
[skills]
sync_tasks = true
install = ["quality-bun-feature-delivery"]

[skills.codex]
generate_openai_yaml = true
force_reload_after_sync = true
task_skill_allow_implicit_invocation = false
"#;
        let cfg: Config = toml::from_str(toml).expect("skills.codex should parse");
        let skills = cfg.skills.expect("skills config expected");
        assert!(skills.sync_tasks);
        assert_eq!(
            skills.install,
            vec!["quality-bun-feature-delivery".to_string()]
        );
        let codex = skills.codex.expect("skills.codex expected");
        assert_eq!(codex.generate_openai_yaml, Some(true));
        assert_eq!(codex.force_reload_after_sync, Some(true));
        assert_eq!(codex.task_skill_allow_implicit_invocation, Some(false));
    }

    #[test]
    fn analytics_config_parses() {
        let toml = r#"
[analytics]
enabled = true
endpoint = "http://127.0.0.1:7331/v1/trace"
sample_rate = 0.5
"#;
        let cfg: Config = toml::from_str(toml).expect("analytics config should parse");
        let analytics = cfg.analytics.expect("analytics config expected");
        assert_eq!(analytics.enabled, Some(true));
        assert_eq!(
            analytics.endpoint.as_deref(),
            Some("http://127.0.0.1:7331/v1/trace")
        );
        assert_eq!(analytics.sample_rate, Some(0.5));
    }
}
