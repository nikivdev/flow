use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use rusqlite::{Connection, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::codex_skill_eval::{self, CodexSkillEvalEvent, CodexSkillOutcomeEvent};
use crate::{ai, codex_text, config, jazz_state, repo_capsule};

const MEMORY_ROOT_ENV: &str = "FLOW_CODEX_MEMORY_ROOT";
const REPO_SYMBOL_INDEX_KIND: &str = "repo_symbols";
const REPO_SYMBOL_INDEX_VERSION: u32 = 1;
const REPO_SESSION_INDEX_KIND: &str = "repo_sessions";
const REPO_SESSION_INDEX_VERSION: u32 = 1;
const MAX_SYMBOL_FILES: usize = 24;
const MAX_SYMBOLS_PER_FILE: usize = 8;
const MAX_SYMBOL_FILE_BYTES: usize = 256 * 1024;
const MAX_SNIPPET_LINES: usize = 4;
const MAX_SNIPPET_CHARS: usize = 220;
const MAX_SESSION_THREADS: usize = 6;
const MAX_SESSION_EXCHANGES_PER_THREAD: usize = 2;
const MAX_SESSION_TEXT_CHARS: usize = 240;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexMemoryStats {
    pub root_dir: String,
    pub db_path: String,
    pub total_events: usize,
    pub total_facts: usize,
    pub skill_eval_events: usize,
    pub skill_eval_outcomes: usize,
    pub latest_recorded_at_unix: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexMemoryRecentEntry {
    pub event_kind: String,
    pub recorded_at_unix: u64,
    pub target_path: Option<String>,
    pub session_id: Option<String>,
    pub route: Option<String>,
    pub query: Option<String>,
    pub success: Option<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexMemorySyncSummary {
    pub total_considered: usize,
    pub inserted: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexMemoryFactHit {
    pub fact_kind: String,
    pub title: String,
    pub body: String,
    pub path_hint: Option<String>,
    pub source_tag: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexMemoryCodeHit {
    pub path: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexMemorySnippetHit {
    pub path: String,
    pub symbol: String,
    pub snippet: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexMemoryQueryResult {
    pub repo_root: String,
    pub query: String,
    pub facts: Vec<CodexMemoryFactHit>,
    pub code_paths: Vec<CodexMemoryCodeHit>,
    pub snippets: Vec<CodexMemorySnippetHit>,
    pub rendered: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoSymbolFact {
    fact_kind: &'static str,
    title: String,
    body: String,
    path_hint: String,
}

#[derive(Debug, Clone)]
struct QueryProfile {
    tokens: Vec<String>,
    code_intent: bool,
    docs_intent: bool,
    explicit_paths: Vec<String>,
}

pub fn root_dir() -> PathBuf {
    if let Ok(path) = std::env::var(MEMORY_ROOT_ENV) {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return config::expand_path(trimmed);
        }
    }
    jazz_state::state_dir().join("codex-memory")
}

pub fn db_path() -> PathBuf {
    root_dir().join("memory.sqlite")
}

pub fn mirror_skill_eval_event(event: &CodexSkillEvalEvent) -> Result<bool> {
    let mut sanitized = event.clone();
    let Some(query) = codex_text::sanitize_codex_query_text(&sanitized.query) else {
        return Ok(false);
    };
    sanitized.query = query;
    let payload = serde_json::to_string(&sanitized).context("failed to encode skill-eval event")?;
    let conn = open_connection()?;
    insert_marshaled(
        &conn,
        "skill_eval_event",
        sanitized.recorded_at_unix,
        Some(sanitized.target_path.as_str()),
        sanitized.session_id.as_deref(),
        sanitized.runtime_token.as_deref(),
        Some(sanitized.route.as_str()),
        Some(sanitized.query.as_str()),
        None,
        &payload,
    )
}

pub fn mirror_skill_outcome_event(outcome: &CodexSkillOutcomeEvent) -> Result<bool> {
    let payload = serde_json::to_string(outcome).context("failed to encode skill outcome")?;
    let conn = open_connection()?;
    insert_marshaled(
        &conn,
        "skill_eval_outcome",
        outcome.recorded_at_unix,
        outcome.target_path.as_deref(),
        outcome.session_id.as_deref(),
        outcome.runtime_token.as_deref(),
        None,
        None,
        Some(outcome.success),
        &payload,
    )
}

pub fn stats() -> Result<CodexMemoryStats> {
    let conn = open_connection()?;
    let mut stmt = conn.prepare(
        "SELECT \
            COUNT(*), \
            (SELECT COUNT(*) FROM codex_memory_facts), \
            COALESCE(SUM(CASE WHEN event_kind = 'skill_eval_event' THEN 1 ELSE 0 END), 0), \
            COALESCE(SUM(CASE WHEN event_kind = 'skill_eval_outcome' THEN 1 ELSE 0 END), 0), \
            MAX(recorded_at_unix) \
         FROM codex_memory_events",
    )?;
    let (total, facts, evals, outcomes, latest): (i64, i64, i64, i64, Option<i64>) = stmt
        .query_row([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
    Ok(CodexMemoryStats {
        root_dir: root_dir().display().to_string(),
        db_path: db_path().display().to_string(),
        total_events: total.max(0) as usize,
        total_facts: facts.max(0) as usize,
        skill_eval_events: evals.max(0) as usize,
        skill_eval_outcomes: outcomes.max(0) as usize,
        latest_recorded_at_unix: latest.map(|value| value.max(0) as u64),
    })
}

pub fn recent(target_path: Option<&Path>, limit: usize) -> Result<Vec<CodexMemoryRecentEntry>> {
    let conn = open_connection()?;
    let mut rows = Vec::new();
    if let Some(target_path) = target_path {
        let target = target_path.display().to_string();
        let target_prefix = format!("{}/%", target.trim_end_matches('/'));
        let mut stmt = conn.prepare(
            "SELECT event_kind, recorded_at_unix, target_path, session_id, route, query, success \
             FROM codex_memory_events \
             WHERE target_path = ?1 OR target_path LIKE ?2 \
             ORDER BY recorded_at_unix DESC \
             LIMIT ?3",
        )?;
        let mut query = stmt.query(params![target, target_prefix, limit as i64])?;
        while let Some(row) = query.next()? {
            rows.push(map_recent_entry(row)?);
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT event_kind, recorded_at_unix, target_path, session_id, route, query, success \
             FROM codex_memory_events \
             ORDER BY recorded_at_unix DESC \
             LIMIT ?1",
        )?;
        let mut query = stmt.query(params![limit as i64])?;
        while let Some(row) = query.next()? {
            rows.push(map_recent_entry(row)?);
        }
    }
    Ok(rows)
}

pub fn sync_from_skill_eval_logs(limit: usize) -> Result<CodexMemorySyncSummary> {
    let mut total_considered = 0usize;
    let mut inserted = 0usize;

    for event in codex_skill_eval::load_events(None, limit)? {
        total_considered += 1;
        if mirror_skill_eval_event(&event)? {
            inserted += 1;
        }
    }
    for outcome in codex_skill_eval::load_outcomes(None, limit)? {
        total_considered += 1;
        if mirror_skill_outcome_event(&outcome)? {
            inserted += 1;
        }
    }

    Ok(CodexMemorySyncSummary {
        total_considered,
        inserted,
        skipped: total_considered.saturating_sub(inserted),
    })
}

pub fn sync_repo_capsule_for_path(path: &Path) -> Result<usize> {
    let capsule = repo_capsule::load_or_refresh_capsule_for_path(path)?;
    mirror_repo_capsule(&capsule)
}

pub fn mirror_repo_capsule(capsule: &repo_capsule::RepoCapsule) -> Result<usize> {
    let conn = open_connection()?;
    let mut changes = 0usize;
    let repo_root = capsule.repo_root.as_str();
    let updated_at_unix = capsule.updated_at_unix;

    changes += upsert_fact(
        &conn,
        repo_root,
        "summary",
        &format!("Summary for {}", capsule.repo_id),
        &capsule.summary,
        None,
        "repo_capsule",
        updated_at_unix,
    )?;

    if !capsule.languages.is_empty() {
        changes += upsert_fact(
            &conn,
            repo_root,
            "languages",
            &format!("Languages in {}", capsule.repo_id),
            &capsule.languages.join(", "),
            None,
            "repo_capsule",
            updated_at_unix,
        )?;
    }

    if !capsule.manifests.is_empty() {
        changes += upsert_fact(
            &conn,
            repo_root,
            "manifests",
            &format!("Manifests in {}", capsule.repo_id),
            &capsule.manifests.join(", "),
            None,
            "repo_capsule",
            updated_at_unix,
        )?;
    }

    for command in &capsule.commands {
        changes += upsert_fact(
            &conn,
            repo_root,
            "command",
            &format!("Command: {}", command),
            &format!("Use `{command}` in {}", capsule.repo_id),
            None,
            "repo_capsule",
            updated_at_unix,
        )?;
    }

    for path in &capsule.important_paths {
        changes += upsert_fact(
            &conn,
            repo_root,
            "important_path",
            &format!("Important path: {}", path),
            &format!("Key file or directory in {}: {}", capsule.repo_id, path),
            Some(path),
            "repo_capsule",
            updated_at_unix,
        )?;
    }

    for hint in &capsule.docs_hints {
        changes += upsert_fact(
            &conn,
            repo_root,
            "docs_hint",
            &format!("Docs hint for {}", capsule.repo_id),
            hint,
            None,
            "repo_capsule",
            updated_at_unix,
        )?;
    }

    changes += sync_repo_symbol_facts(&conn, capsule)?;
    if let Ok(session_changes) = sync_repo_session_facts(&conn, capsule) {
        changes += session_changes;
    }

    Ok(changes)
}

pub fn query_repo_facts(
    path: &Path,
    query: &str,
    limit: usize,
) -> Result<Option<CodexMemoryQueryResult>> {
    let capsule = repo_capsule::load_or_refresh_capsule_for_path(path)?;
    let _ = mirror_repo_capsule(&capsule);
    let profile = build_query_profile(query);
    let conn = open_connection()?;
    let mut stmt = conn.prepare(
        "SELECT fact_kind, title, body, path_hint, source_tag \
         FROM codex_memory_facts \
         WHERE target_path = ?1 \
         ORDER BY updated_at_unix DESC",
    )?;
    let mut rows = stmt.query(params![capsule.repo_root.as_str()])?;
    let mut hits = Vec::new();
    while let Some(row) = rows.next()? {
        let fact_kind: String = row.get(0)?;
        let title: String = row.get(1)?;
        let body: String = row.get(2)?;
        let path_hint: Option<String> = row.get(3)?;
        let source_tag: String = row.get(4)?;
        let score = fact_score(&profile, &fact_kind, &title, &body, path_hint.as_deref());
        if score <= 0.0 {
            continue;
        }
        hits.push(CodexMemoryFactHit {
            fact_kind,
            title,
            body,
            path_hint,
            source_tag,
            score,
        });
    }

    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut code_paths = search_code_paths(Path::new(&capsule.repo_root), &profile, limit);
    let dynamic_symbols =
        search_symbols_for_code_paths(Path::new(&capsule.repo_root), &code_paths, &profile, limit);
    merge_dynamic_symbol_hits(&mut hits, dynamic_symbols);
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits.truncate(limit);
    if hits.is_empty() && code_paths.is_empty() {
        return Ok(None);
    }
    let snippets = extract_symbol_snippets(Path::new(&capsule.repo_root), &hits, 2);

    if !hits.is_empty() {
        let hinted_paths: std::collections::BTreeSet<_> = hits
            .iter()
            .filter_map(|hit| hit.path_hint.as_deref())
            .collect();
        code_paths.retain(|hit| !hinted_paths.contains(hit.path.as_str()));
    }

    let rendered = render_query_result(
        &capsule.repo_root,
        query,
        &profile,
        &hits,
        &code_paths,
        &snippets,
    );
    Ok(Some(CodexMemoryQueryResult {
        repo_root: capsule.repo_root,
        query: query.trim().to_string(),
        facts: hits,
        code_paths,
        snippets,
        rendered,
    }))
}

fn open_connection() -> Result<Connection> {
    open_connection_at(&db_path())
}

fn open_connection_at(path: &Path) -> Result<Connection> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing parent for {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let conn =
        Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    conn.busy_timeout(Duration::from_millis(1500))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS codex_memory_events (
            event_key TEXT PRIMARY KEY,
            event_kind TEXT NOT NULL,
            recorded_at_unix INTEGER NOT NULL,
            target_path TEXT,
            session_id TEXT,
            runtime_token TEXT,
            route TEXT,
            query TEXT,
            success REAL,
            payload_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_codex_memory_events_target_time
            ON codex_memory_events(target_path, recorded_at_unix DESC);
        CREATE INDEX IF NOT EXISTS idx_codex_memory_events_session_time
            ON codex_memory_events(session_id, recorded_at_unix DESC);
        CREATE INDEX IF NOT EXISTS idx_codex_memory_events_kind_time
            ON codex_memory_events(event_kind, recorded_at_unix DESC);
        CREATE TABLE IF NOT EXISTS codex_memory_facts (
            fact_key TEXT PRIMARY KEY,
            target_path TEXT NOT NULL,
            fact_kind TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            path_hint TEXT,
            source_tag TEXT NOT NULL,
            updated_at_unix INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_codex_memory_facts_target_time
            ON codex_memory_facts(target_path, updated_at_unix DESC);
        CREATE INDEX IF NOT EXISTS idx_codex_memory_facts_kind
            ON codex_memory_facts(fact_kind);
        CREATE TABLE IF NOT EXISTS codex_memory_indexes (
            target_path TEXT NOT NULL,
            index_kind TEXT NOT NULL,
            version INTEGER NOT NULL,
            source_updated_at_unix INTEGER NOT NULL,
            updated_at_unix INTEGER NOT NULL,
            PRIMARY KEY(target_path, index_kind)
        );",
    )?;
    Ok(conn)
}

fn insert_marshaled(
    conn: &Connection,
    event_kind: &str,
    recorded_at_unix: u64,
    target_path: Option<&str>,
    session_id: Option<&str>,
    runtime_token: Option<&str>,
    route: Option<&str>,
    query: Option<&str>,
    success: Option<f64>,
    payload_json: &str,
) -> Result<bool> {
    let key = event_key(event_kind, payload_json);
    let changed = conn.execute(
        "INSERT OR IGNORE INTO codex_memory_events (
            event_key,
            event_kind,
            recorded_at_unix,
            target_path,
            session_id,
            runtime_token,
            route,
            query,
            success,
            payload_json
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            key,
            event_kind,
            recorded_at_unix as i64,
            target_path,
            session_id,
            runtime_token,
            route,
            query,
            success,
            payload_json
        ],
    )?;
    Ok(changed > 0)
}

fn event_key(event_kind: &str, payload_json: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(event_kind.as_bytes());
    hasher.update([0u8]);
    hasher.update(payload_json.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn fact_key(
    target_path: &str,
    fact_kind: &str,
    title: &str,
    body: &str,
    path_hint: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(target_path.as_bytes());
    hasher.update([0u8]);
    hasher.update(fact_kind.as_bytes());
    hasher.update([0u8]);
    hasher.update(title.as_bytes());
    hasher.update([0u8]);
    hasher.update(body.as_bytes());
    hasher.update([0u8]);
    hasher.update(path_hint.unwrap_or("").as_bytes());
    format!("{:x}", hasher.finalize())
}

fn upsert_fact(
    conn: &Connection,
    target_path: &str,
    fact_kind: &str,
    title: &str,
    body: &str,
    path_hint: Option<&str>,
    source_tag: &str,
    updated_at_unix: u64,
) -> Result<usize> {
    let key = fact_key(target_path, fact_kind, title, body, path_hint);
    let changed = conn.execute(
        "INSERT INTO codex_memory_facts (
            fact_key, target_path, fact_kind, title, body, path_hint, source_tag, updated_at_unix
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ON CONFLICT(fact_key) DO UPDATE SET
            target_path = excluded.target_path,
            fact_kind = excluded.fact_kind,
            title = excluded.title,
            body = excluded.body,
            path_hint = excluded.path_hint,
            source_tag = excluded.source_tag,
            updated_at_unix = excluded.updated_at_unix",
        params![
            key,
            target_path,
            fact_kind,
            title,
            body,
            path_hint,
            source_tag,
            updated_at_unix as i64,
        ],
    )?;
    Ok(changed)
}

fn fact_score(
    profile: &QueryProfile,
    fact_kind: &str,
    title: &str,
    body: &str,
    path_hint: Option<&str>,
) -> f64 {
    if profile.tokens.is_empty() && profile.explicit_paths.is_empty() {
        return 0.0;
    }
    let title_lower = title.to_ascii_lowercase();
    let body_lower = body.to_ascii_lowercase();
    let path_lower = path_hint.unwrap_or("").to_ascii_lowercase();
    let kind_lower = fact_kind.to_ascii_lowercase();

    let mut score = 0.0;
    for token in &profile.tokens {
        if title_lower.contains(token.as_str()) {
            score += 3.0;
        }
        if body_lower.contains(token.as_str()) {
            score += 1.5;
        }
        if path_lower.contains(token.as_str()) {
            score += 2.0;
        }
        if kind_lower.contains(token.as_str()) {
            score += 0.5;
        }
    }
    for explicit_path in &profile.explicit_paths {
        if path_lower == *explicit_path {
            score += 10.0;
        } else if path_lower.contains(explicit_path) {
            score += 6.0;
        }
    }

    if profile.code_intent {
        match fact_kind {
            "symbol" => score += 5.0,
            "entrypoint" => score += 3.0,
            "session_exchange" => score += 2.5,
            "session_intent" => score += 1.5,
            "session_recent" => score += 1.0,
            "important_path" | "command" => score += 1.5,
            "doc_heading" | "docs_hint" | "summary" => score -= 2.0,
            _ => {}
        }
        if path_lower.starts_with("src/")
            || path_lower.ends_with(".rs")
            || path_lower.ends_with(".ts")
        {
            score += 1.0;
        }
    }
    if profile.docs_intent {
        match fact_kind {
            "doc_heading" => score += 4.0,
            "docs_hint" | "summary" => score += 2.0,
            "session_recent" => score += 0.5,
            "symbol" => score -= 2.0,
            "session_exchange" => score -= 1.0,
            _ => {}
        }
        if path_lower.starts_with("docs/")
            || path_lower.ends_with(".md")
            || path_lower.ends_with(".mdx")
        {
            score += 1.5;
        }
    }
    score
}

fn tokenize_query(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-' && ch != '/')
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase())
        .filter(|part| {
            part.len() >= 3
                && !matches!(
                    part.as_str(),
                    "see"
                        | "with"
                        | "this"
                        | "that"
                        | "from"
                        | "into"
                        | "repo"
                        | "code"
                        | "work"
                        | "what"
                        | "latest"
                        | "codex"
                )
        })
        .collect()
}

fn build_query_profile(query: &str) -> QueryProfile {
    let query_lower = query.to_ascii_lowercase();
    let tokens = tokenize_query(query);
    let code_intent = tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "implement"
                | "fix"
                | "refactor"
                | "edit"
                | "change"
                | "update"
                | "patch"
                | "function"
                | "struct"
                | "class"
                | "type"
                | "module"
                | "file"
                | "bug"
                | "perf"
                | "performance"
                | "optimize"
        )
    }) || query_lower.contains("src/")
        || query_lower.contains(".rs")
        || query_lower.contains(".ts")
        || query_lower.contains(".py");
    let docs_intent = tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "summarize" | "summary" | "roadmap" | "docs" | "document" | "guide" | "readme"
        )
    });
    let explicit_paths = extract_explicit_paths(&query_lower);

    QueryProfile {
        tokens,
        code_intent,
        docs_intent,
        explicit_paths,
    }
}

fn extract_explicit_paths(query_lower: &str) -> Vec<String> {
    query_lower
        .split_whitespace()
        .filter_map(|part| {
            let trimmed = part.trim_matches(|ch: char| {
                matches!(ch, ',' | '.' | ':' | ';' | ')' | '(' | '"' | '\'')
            });
            if trimmed.contains('/')
                && (trimmed.starts_with("src/")
                    || trimmed.starts_with("docs/")
                    || trimmed.starts_with("crates/")
                    || trimmed.starts_with("scripts/")
                    || trimmed.ends_with(".rs")
                    || trimmed.ends_with(".ts")
                    || trimmed.ends_with(".tsx")
                    || trimmed.ends_with(".py")
                    || trimmed.ends_with(".md")
                    || trimmed.ends_with(".mdx"))
            {
                Some(trimmed.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn trim_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let keep = limit.saturating_sub(3);
    value.chars().take(keep).collect::<String>() + "..."
}

fn session_label(session_id: &str, title: Option<&str>) -> String {
    if let Some(title) = title
        && !title.trim().is_empty()
    {
        return trim_chars(title.trim(), 80);
    }
    let short = session_id.chars().take(8).collect::<String>();
    format!("session {}", short)
}

fn session_summary_body(row: &ai::CodexRecoverRow) -> String {
    let mut parts = Vec::new();
    if let Some(title) = row
        .title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        parts.push(format!(
            "Title: {}",
            trim_chars(title.trim(), MAX_SESSION_TEXT_CHARS)
        ));
    }
    if let Some(branch) = row
        .git_branch
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        parts.push(format!("Branch: {}", branch.trim()));
    }
    if let Some(first) = row
        .first_user_message
        .as_deref()
        .and_then(codex_text::sanitize_codex_query_text)
    {
        parts.push(format!(
            "First user message: {}",
            trim_chars(&first, MAX_SESSION_TEXT_CHARS)
        ));
    }
    if parts.is_empty() {
        format!("Recent Codex session in {}", row.cwd)
    } else {
        parts.join(" | ")
    }
}

fn sync_repo_symbol_facts(conn: &Connection, capsule: &repo_capsule::RepoCapsule) -> Result<usize> {
    if index_is_fresh(
        conn,
        &capsule.repo_root,
        REPO_SYMBOL_INDEX_KIND,
        REPO_SYMBOL_INDEX_VERSION,
        capsule.updated_at_unix,
    )? {
        return Ok(0);
    }

    conn.execute(
        "DELETE FROM codex_memory_facts WHERE target_path = ?1 AND source_tag = 'repo_symbols'",
        params![capsule.repo_root.as_str()],
    )?;

    let repo_root = Path::new(&capsule.repo_root);
    let mut changes = 0usize;
    for fact in collect_repo_symbol_facts(repo_root, capsule)? {
        changes += upsert_fact(
            conn,
            &capsule.repo_root,
            fact.fact_kind,
            &fact.title,
            &fact.body,
            Some(&fact.path_hint),
            "repo_symbols",
            capsule.updated_at_unix,
        )?;
    }

    mark_index_fresh(
        conn,
        &capsule.repo_root,
        REPO_SYMBOL_INDEX_KIND,
        REPO_SYMBOL_INDEX_VERSION,
        capsule.updated_at_unix,
    )?;

    Ok(changes)
}

fn sync_repo_session_facts(
    conn: &Connection,
    capsule: &repo_capsule::RepoCapsule,
) -> Result<usize> {
    let repo_root = Path::new(&capsule.repo_root);
    let recent = ai::read_recent_codex_threads_local(repo_root, false, MAX_SESSION_THREADS, None)?;
    let source_updated_at = recent
        .iter()
        .map(|row| row.updated_at.max(0) as u64)
        .max()
        .unwrap_or(0);

    if index_is_fresh(
        conn,
        &capsule.repo_root,
        REPO_SESSION_INDEX_KIND,
        REPO_SESSION_INDEX_VERSION,
        source_updated_at,
    )? {
        return Ok(0);
    }

    conn.execute(
        "DELETE FROM codex_memory_facts WHERE target_path = ?1 AND source_tag = 'repo_sessions'",
        params![capsule.repo_root.as_str()],
    )?;

    let mut changes = 0usize;
    for row in recent {
        let session_label = session_label(&row.id, row.title.as_deref());
        let updated_at_unix = row.updated_at.max(0) as u64;
        changes += upsert_fact(
            conn,
            &capsule.repo_root,
            "session_recent",
            &format!("Recent Codex session: {}", session_label),
            &session_summary_body(&row),
            None,
            "repo_sessions",
            updated_at_unix,
        )?;

        if let Some(intent) = row
            .first_user_message
            .as_deref()
            .and_then(codex_text::sanitize_codex_query_text)
        {
            changes += upsert_fact(
                conn,
                &capsule.repo_root,
                "session_intent",
                &format!("Recent Codex intent: {}", session_label),
                &trim_chars(&intent, MAX_SESSION_TEXT_CHARS),
                None,
                "repo_sessions",
                updated_at_unix,
            )?;
        }

        if let Ok(exchanges) =
            ai::read_codex_memory_exchanges(&row.id, MAX_SESSION_EXCHANGES_PER_THREAD)
        {
            for (index, (user, assistant)) in exchanges.into_iter().enumerate() {
                let body = format!(
                    "User: {}\nAssistant: {}",
                    trim_chars(&user, MAX_SESSION_TEXT_CHARS),
                    trim_chars(&assistant, MAX_SESSION_TEXT_CHARS)
                );
                changes += upsert_fact(
                    conn,
                    &capsule.repo_root,
                    "session_exchange",
                    &format!("Recent Codex exchange {}: {}", index + 1, session_label),
                    &body,
                    None,
                    "repo_sessions",
                    updated_at_unix,
                )?;
            }
        }
    }

    mark_index_fresh(
        conn,
        &capsule.repo_root,
        REPO_SESSION_INDEX_KIND,
        REPO_SESSION_INDEX_VERSION,
        source_updated_at,
    )?;

    Ok(changes)
}

fn collect_repo_symbol_facts(
    repo_root: &Path,
    capsule: &repo_capsule::RepoCapsule,
) -> Result<Vec<RepoSymbolFact>> {
    let candidates = collect_symbol_candidate_paths(repo_root, capsule);
    let mut facts = Vec::new();

    for relative_path in candidates {
        let absolute = repo_root.join(&relative_path);
        let Ok(metadata) = fs::metadata(&absolute) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() as usize > MAX_SYMBOL_FILE_BYTES {
            continue;
        }
        if let Some(entrypoint_body) = entrypoint_body_for_path(&relative_path) {
            facts.push(RepoSymbolFact {
                fact_kind: "entrypoint",
                title: format!("Entrypoint: {}", relative_path),
                body: format!(
                    "{} in {}: {}",
                    entrypoint_body, capsule.repo_id, relative_path
                ),
                path_hint: relative_path.clone(),
            });
        }

        let Ok(content) = fs::read_to_string(&absolute) else {
            continue;
        };
        facts.extend(extract_symbol_facts(&relative_path, &content));
    }

    Ok(facts)
}

fn collect_symbol_candidate_paths(
    repo_root: &Path,
    capsule: &repo_capsule::RepoCapsule,
) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut candidates = Vec::new();

    for path in &capsule.important_paths {
        let absolute = repo_root.join(path);
        if absolute.is_file() && matches_code_extension(&absolute) && seen.insert(path.clone()) {
            candidates.push(path.clone());
        }
    }

    let preferred = [
        "src/main.rs",
        "src/lib.rs",
        "src/mod.rs",
        "src/index.ts",
        "src/index.tsx",
        "src/main.ts",
        "src/main.tsx",
        "src/app.ts",
        "src/app.tsx",
        "src/App.tsx",
        "main.py",
        "app.py",
        "__init__.py",
        "index.ts",
        "index.js",
        "README.md",
        "AGENTS.md",
        "flow.toml",
    ];
    for path in preferred {
        let absolute = repo_root.join(path);
        if absolute.is_file() && seen.insert(path.to_string()) {
            candidates.push(path.to_string());
        }
    }

    let mut builder = WalkBuilder::new(repo_root);
    builder
        .standard_filters(true)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .max_depth(Some(4));

    let mut considered = 0usize;
    let mut scored = Vec::new();
    for entry in builder.build() {
        let Ok(entry) = entry else {
            continue;
        };
        if considered >= 600 {
            break;
        }
        let path = entry.path();
        if !path.is_file() || !matches_code_extension(path) {
            continue;
        }
        considered += 1;
        let Some(relative) = path.strip_prefix(repo_root).ok() else {
            continue;
        };
        let relative_text = relative.display().to_string();
        let score = entrypoint_path_score(&relative_text);
        if score <= 0.0 || seen.contains(&relative_text) {
            continue;
        }
        scored.push((score, relative_text));
    }

    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.len().cmp(&b.1.len()))
            .then_with(|| a.1.cmp(&b.1))
    });

    for (_, path) in scored {
        if seen.insert(path.clone()) {
            candidates.push(path);
        }
        if candidates.len() >= MAX_SYMBOL_FILES {
            break;
        }
    }

    candidates.truncate(MAX_SYMBOL_FILES);
    candidates
}

fn entrypoint_path_score(relative_path: &str) -> f64 {
    let path_lower = relative_path.to_ascii_lowercase();
    let file_name = Path::new(relative_path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut score = 0.0;
    if matches!(
        file_name.as_str(),
        "main.rs"
            | "lib.rs"
            | "mod.rs"
            | "index.ts"
            | "index.tsx"
            | "index.js"
            | "main.ts"
            | "main.tsx"
            | "app.ts"
            | "app.tsx"
            | "app.py"
            | "main.py"
            | "__init__.py"
            | "readme.md"
            | "agents.md"
            | "flow.toml"
    ) {
        score += 6.0;
    }
    if path_lower.starts_with("src/") {
        score += 2.0;
    } else if path_lower.starts_with("app/") || path_lower.starts_with("lib/") {
        score += 1.5;
    } else if path_lower.starts_with("docs/") {
        score += 1.0;
    }
    if path_lower.contains("/cli/") || path_lower.contains("/bin/") {
        score += 1.0;
    }
    score
}

fn entrypoint_body_for_path(relative_path: &str) -> Option<&'static str> {
    let path_lower = relative_path.to_ascii_lowercase();
    let file_name = Path::new(relative_path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if matches!(
        file_name.as_str(),
        "main.rs" | "main.ts" | "main.tsx" | "main.py" | "index.ts" | "index.tsx" | "index.js"
    ) {
        return Some("Likely runtime entrypoint");
    }
    if matches!(file_name.as_str(), "lib.rs" | "__init__.py") {
        return Some("Likely library entrypoint");
    }
    if matches!(
        file_name.as_str(),
        "app.ts" | "app.tsx" | "app.py" | "app.js"
    ) {
        return Some("Likely application entrypoint");
    }
    if file_name == "flow.toml" {
        return Some("Flow project entrypoint/config");
    }
    if path_lower.starts_with("docs/") || file_name == "readme.md" || file_name == "agents.md" {
        return Some("Likely docs/operating guide entrypoint");
    }
    None
}

fn extract_symbol_facts(relative_path: &str, content: &str) -> Vec<RepoSymbolFact> {
    let extension = Path::new(relative_path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let mut facts = match extension {
        "rs" => extract_rust_symbol_facts(relative_path, content),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => {
            extract_ts_symbol_facts(relative_path, content)
        }
        "py" => extract_python_symbol_facts(relative_path, content),
        "go" => extract_go_symbol_facts(relative_path, content),
        "md" | "mdx" => extract_markdown_heading_facts(relative_path, content),
        _ => Vec::new(),
    };
    facts.truncate(MAX_SYMBOLS_PER_FILE);
    facts
}

fn extract_rust_symbol_facts(relative_path: &str, content: &str) -> Vec<RepoSymbolFact> {
    let mut facts = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        let (kind, name) = if let Some(name) = parse_prefixed_name(trimmed, &["pub fn ", "fn "]) {
            ("fn", name)
        } else if let Some(name) =
            parse_prefixed_name(trimmed, &["pub struct ", "struct ", "pub(crate) struct "])
        {
            ("struct", name)
        } else if let Some(name) = parse_prefixed_name(trimmed, &["pub enum ", "enum "]) {
            ("enum", name)
        } else if let Some(name) = parse_prefixed_name(trimmed, &["pub trait ", "trait "]) {
            ("trait", name)
        } else if let Some(name) = parse_prefixed_name(trimmed, &["pub mod ", "mod "]) {
            ("mod", name)
        } else {
            continue;
        };
        facts.push(symbol_fact(relative_path, kind, &name));
        if facts.len() >= MAX_SYMBOLS_PER_FILE {
            break;
        }
    }
    facts
}

fn extract_ts_symbol_facts(relative_path: &str, content: &str) -> Vec<RepoSymbolFact> {
    let mut facts = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        let (kind, name) = if let Some(name) = parse_prefixed_name(
            trimmed,
            &[
                "export async function ",
                "export function ",
                "async function ",
                "function ",
            ],
        ) {
            ("function", name)
        } else if let Some(name) = parse_prefixed_name(
            trimmed,
            &["export class ", "class ", "export default class "],
        ) {
            ("class", name)
        } else if let Some(name) =
            parse_prefixed_name(trimmed, &["export interface ", "interface "])
        {
            ("interface", name)
        } else if let Some(name) = parse_prefixed_name(trimmed, &["export type ", "type "]) {
            ("type", name)
        } else if let Some(name) = parse_const_name(trimmed) {
            ("const", name)
        } else {
            continue;
        };
        facts.push(symbol_fact(relative_path, kind, &name));
        if facts.len() >= MAX_SYMBOLS_PER_FILE {
            break;
        }
    }
    facts
}

fn extract_python_symbol_facts(relative_path: &str, content: &str) -> Vec<RepoSymbolFact> {
    let mut facts = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        let (kind, name) = if let Some(name) = parse_prefixed_name(trimmed, &["def ", "async def "])
        {
            ("function", name)
        } else if let Some(name) = parse_prefixed_name(trimmed, &["class "]) {
            ("class", name)
        } else {
            continue;
        };
        facts.push(symbol_fact(relative_path, kind, &name));
        if facts.len() >= MAX_SYMBOLS_PER_FILE {
            break;
        }
    }
    facts
}

fn extract_go_symbol_facts(relative_path: &str, content: &str) -> Vec<RepoSymbolFact> {
    let mut facts = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        let (kind, name) = if let Some(name) = parse_go_func_name(trimmed) {
            ("func", name)
        } else if let Some(name) = parse_prefixed_name(trimmed, &["type "]) {
            ("type", name)
        } else {
            continue;
        };
        facts.push(symbol_fact(relative_path, kind, &name));
        if facts.len() >= MAX_SYMBOLS_PER_FILE {
            break;
        }
    }
    facts
}

fn extract_markdown_heading_facts(relative_path: &str, content: &str) -> Vec<RepoSymbolFact> {
    let mut facts = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('#') {
            continue;
        }
        let heading = trimmed.trim_start_matches('#').trim();
        if heading.len() < 3 {
            continue;
        }
        facts.push(RepoSymbolFact {
            fact_kind: "doc_heading",
            title: format!("Doc heading: {}", heading),
            body: format!("Heading in {}: {}", relative_path, heading),
            path_hint: relative_path.to_string(),
        });
        if facts.len() >= 4 {
            break;
        }
    }
    facts
}

fn symbol_fact(relative_path: &str, kind: &str, name: &str) -> RepoSymbolFact {
    RepoSymbolFact {
        fact_kind: "symbol",
        title: format!("Symbol: {}", name),
        body: format!("{} {} in {}", kind, name, relative_path),
        path_hint: relative_path.to_string(),
    }
}

fn parse_prefixed_name(trimmed: &str, prefixes: &[&str]) -> Option<String> {
    for prefix in prefixes {
        let Some(rest) = trimmed.strip_prefix(prefix) else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
            .collect();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

fn parse_const_name(trimmed: &str) -> Option<String> {
    let rest = if let Some(value) = trimmed.strip_prefix("export const ") {
        value
    } else if let Some(value) = trimmed.strip_prefix("const ") {
        value
    } else {
        return None;
    };
    let name: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '$')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

fn parse_go_func_name(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("func ")?;
    let rest = if rest.starts_with('(') {
        let idx = rest.find(')')?;
        rest.get(idx + 1..)?.trim_start()
    } else {
        rest
    };
    let name: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

fn index_is_fresh(
    conn: &Connection,
    target_path: &str,
    index_kind: &str,
    version: u32,
    source_updated_at_unix: u64,
) -> Result<bool> {
    let row = conn.query_row(
        "SELECT version, source_updated_at_unix FROM codex_memory_indexes \
         WHERE target_path = ?1 AND index_kind = ?2",
        params![target_path, index_kind],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    );
    match row {
        Ok((stored_version, stored_source_updated)) => Ok(stored_version == version as i64
            && stored_source_updated == source_updated_at_unix as i64),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn mark_index_fresh(
    conn: &Connection,
    target_path: &str,
    index_kind: &str,
    version: u32,
    source_updated_at_unix: u64,
) -> Result<()> {
    let updated_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(source_updated_at_unix);
    conn.execute(
        "INSERT INTO codex_memory_indexes (
            target_path, index_kind, version, source_updated_at_unix, updated_at_unix
        ) VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(target_path, index_kind) DO UPDATE SET
            version = excluded.version,
            source_updated_at_unix = excluded.source_updated_at_unix,
            updated_at_unix = excluded.updated_at_unix",
        params![
            target_path,
            index_kind,
            version as i64,
            source_updated_at_unix as i64,
            updated_at_unix as i64,
        ],
    )?;
    Ok(())
}

fn search_code_paths(
    repo_root: &Path,
    profile: &QueryProfile,
    limit: usize,
) -> Vec<CodexMemoryCodeHit> {
    if (profile.tokens.is_empty() && profile.explicit_paths.is_empty())
        || limit == 0
        || !repo_root.exists()
    {
        return Vec::new();
    }

    let mut builder = WalkBuilder::new(repo_root);
    builder
        .standard_filters(true)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .max_depth(Some(8));

    let mut considered = 0usize;
    let mut hits = Vec::new();

    for entry in builder.build() {
        let Ok(entry) = entry else {
            continue;
        };
        if considered >= 2000 {
            break;
        }
        let path = entry.path();
        if !path.is_file() || !matches_code_extension(path) {
            continue;
        }
        considered += 1;
        let Some(relative) = path.strip_prefix(repo_root).ok() else {
            continue;
        };
        let relative_text = relative.display().to_string();
        let score = score_code_path(&relative_text, profile);
        if score <= 0.0 {
            continue;
        }
        hits.push(CodexMemoryCodeHit {
            path: relative_text,
            score,
        });
    }

    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.len().cmp(&b.path.len()))
            .then_with(|| a.path.cmp(&b.path))
    });
    hits.truncate(limit);
    hits
}

fn search_symbols_for_code_paths(
    repo_root: &Path,
    code_paths: &[CodexMemoryCodeHit],
    profile: &QueryProfile,
    limit: usize,
) -> Vec<CodexMemoryFactHit> {
    let mut hits = Vec::new();
    for code_path in code_paths.iter().take(limit.min(4)) {
        let extension = Path::new(&code_path.path)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if matches!(extension, "md" | "mdx") {
            continue;
        }
        let absolute = repo_root.join(&code_path.path);
        let Ok(metadata) = fs::metadata(&absolute) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() as usize > MAX_SYMBOL_FILE_BYTES {
            continue;
        }
        let Ok(content) = fs::read_to_string(&absolute) else {
            continue;
        };
        for fact in extract_symbol_facts(&code_path.path, &content) {
            let score = fact_score(
                profile,
                fact.fact_kind,
                &fact.title,
                &fact.body,
                Some(&fact.path_hint),
            ) + (code_path.score * 0.6);
            if score <= 0.0 {
                continue;
            }
            hits.push(CodexMemoryFactHit {
                fact_kind: fact.fact_kind.to_string(),
                title: fact.title,
                body: fact.body,
                path_hint: Some(fact.path_hint),
                source_tag: "live_symbol".to_string(),
                score,
            });
        }
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits.truncate(limit);
    hits
}

fn merge_dynamic_symbol_hits(
    hits: &mut Vec<CodexMemoryFactHit>,
    dynamic_symbols: Vec<CodexMemoryFactHit>,
) {
    let mut seen = std::collections::BTreeSet::new();
    for hit in hits.iter() {
        seen.insert((
            hit.fact_kind.clone(),
            hit.title.clone(),
            hit.path_hint.clone().unwrap_or_default(),
        ));
    }
    for hit in dynamic_symbols {
        let key = (
            hit.fact_kind.clone(),
            hit.title.clone(),
            hit.path_hint.clone().unwrap_or_default(),
        );
        if seen.insert(key) {
            hits.push(hit);
        }
    }
}

fn extract_symbol_snippets(
    repo_root: &Path,
    facts: &[CodexMemoryFactHit],
    limit: usize,
) -> Vec<CodexMemorySnippetHit> {
    let mut snippets = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for fact in facts {
        if snippets.len() >= limit {
            break;
        }
        if fact.fact_kind != "symbol" {
            continue;
        }
        let Some(path) = fact.path_hint.as_deref() else {
            continue;
        };
        if !seen.insert(path.to_string()) {
            continue;
        }
        let Some(symbol_name) = fact.title.strip_prefix("Symbol: ").map(str::trim) else {
            continue;
        };
        let absolute = repo_root.join(path);
        let Ok(metadata) = fs::metadata(&absolute) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() as usize > MAX_SYMBOL_FILE_BYTES {
            continue;
        }
        let Ok(content) = fs::read_to_string(&absolute) else {
            continue;
        };
        let Some(snippet) = find_symbol_snippet(&content, symbol_name) else {
            continue;
        };
        snippets.push(CodexMemorySnippetHit {
            path: path.to_string(),
            symbol: symbol_name.to_string(),
            snippet,
            score: fact.score,
        });
    }

    snippets
}

fn find_symbol_snippet(content: &str, symbol_name: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let symbol_lower = symbol_name.to_ascii_lowercase();

    let start_idx = lines.iter().position(|line| {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        lower.contains(&symbol_lower)
            && (trimmed.starts_with("pub ")
                || trimmed.starts_with("fn ")
                || trimmed.starts_with("struct ")
                || trimmed.starts_with("enum ")
                || trimmed.starts_with("trait ")
                || trimmed.starts_with("class ")
                || trimmed.starts_with("interface ")
                || trimmed.starts_with("type ")
                || trimmed.starts_with("export ")
                || trimmed.starts_with("async ")
                || trimmed.starts_with("def ")
                || trimmed.starts_with("func "))
    })?;

    let mut excerpt = Vec::new();
    for line in lines.iter().skip(start_idx).take(MAX_SNIPPET_LINES) {
        let trimmed = line.trim_end();
        if trimmed.is_empty() && !excerpt.is_empty() {
            break;
        }
        if !trimmed.is_empty() {
            excerpt.push(trimmed.trim().to_string());
        }
    }
    if excerpt.is_empty() {
        return None;
    }
    Some(trim_chars(&excerpt.join(" | "), MAX_SNIPPET_CHARS))
}

fn matches_code_extension(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    if matches!(name, "README.md" | "README.mdx" | "AGENTS.md" | "flow.toml") {
        return true;
    }

    let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
        return false;
    };
    matches!(
        extension,
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "py"
            | "go"
            | "md"
            | "mdx"
            | "toml"
            | "json"
            | "jsonc"
            | "yaml"
            | "yml"
            | "moon"
            | "cpp"
            | "cc"
            | "c"
            | "h"
            | "hpp"
            | "java"
            | "kt"
            | "swift"
    )
}

fn score_code_path(relative_path: &str, profile: &QueryProfile) -> f64 {
    let path_lower = relative_path.to_ascii_lowercase();
    let file_name = Path::new(relative_path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut score = 0.0;
    for token in &profile.tokens {
        if file_name == *token || file_name.starts_with(&format!("{token}.")) {
            score += 6.0;
            continue;
        }
        if file_name.contains(token) {
            score += 4.0;
        }
        if path_lower.contains(&format!("/{token}/")) {
            score += 3.0;
        } else if path_lower.contains(token) {
            score += 2.0;
        }
    }
    for explicit_path in &profile.explicit_paths {
        if path_lower == *explicit_path {
            score += 14.0;
        } else if path_lower.contains(explicit_path) {
            score += 8.0;
        }
    }

    if profile.code_intent {
        if path_lower.starts_with("src/") || path_lower.contains("/src/") {
            score += 2.0;
        } else if path_lower.starts_with("crates/") || path_lower.starts_with("app/") {
            score += 1.0;
        } else if path_lower.starts_with("docs/") {
            score -= 1.0;
        }
    } else if profile.docs_intent {
        if path_lower.starts_with("docs/")
            || path_lower.ends_with(".md")
            || path_lower.ends_with(".mdx")
        {
            score += 2.0;
        } else if path_lower.starts_with("src/") {
            score -= 0.5;
        }
    } else if path_lower.starts_with("src/") {
        score += 0.5;
    } else if path_lower.starts_with("docs/") {
        score += 0.3;
    }
    score
}

fn render_query_result(
    repo_root: &str,
    query: &str,
    profile: &QueryProfile,
    facts: &[CodexMemoryFactHit],
    code_paths: &[CodexMemoryCodeHit],
    snippets: &[CodexMemorySnippetHit],
) -> String {
    let mut lines = vec![format!("- Memory repo root: {}", repo_root)];
    lines.push(format!("- Memory query: {}", query.trim()));
    for fact in select_render_fact_hits(facts, profile, 6) {
        let mut line = format!("- {}: {}", fact.fact_kind, fact.body);
        if let Some(path) = fact.path_hint.as_deref() {
            line.push_str(&format!(" ({})", path));
        }
        lines.push(line);
    }
    for snippet in snippets {
        lines.push(format!(
            "- snippet {}::{} => {}",
            snippet.path, snippet.symbol, snippet.snippet
        ));
    }
    for hit in code_paths {
        lines.push(format!("- code_path: {}", hit.path));
    }
    lines.join("\n")
}

fn select_render_fact_hits<'a>(
    facts: &'a [CodexMemoryFactHit],
    profile: &QueryProfile,
    limit: usize,
) -> Vec<&'a CodexMemoryFactHit> {
    let mut selected = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    let preferred_kinds: &[&str] = if profile.code_intent {
        &[
            "symbol",
            "entrypoint",
            "session_exchange",
            "session_intent",
            "important_path",
            "command",
        ]
    } else if profile.docs_intent {
        &["doc_heading", "docs_hint", "summary", "important_path"]
    } else {
        &[
            "session_intent",
            "session_exchange",
            "symbol",
            "entrypoint",
            "command",
            "important_path",
        ]
    };

    for preferred_kind in preferred_kinds {
        for fact in facts {
            if fact.fact_kind != *preferred_kind {
                continue;
            }
            let key = (
                fact.fact_kind.as_str(),
                fact.title.as_str(),
                fact.path_hint.as_deref().unwrap_or(""),
            );
            if seen.insert(key) {
                selected.push(fact);
                break;
            }
        }
    }

    for fact in facts {
        if selected.len() >= limit {
            break;
        }
        if profile.code_intent
            && matches!(
                fact.fact_kind.as_str(),
                "doc_heading" | "docs_hint" | "summary"
            )
            && selected
                .iter()
                .filter(|item| item.fact_kind == "doc_heading")
                .count()
                >= 1
        {
            continue;
        }
        if matches!(fact.fact_kind.as_str(), "doc_heading" | "docs_hint")
            && selected
                .iter()
                .filter(|item| item.fact_kind == "doc_heading")
                .count()
                >= 2
        {
            continue;
        }
        let key = (
            fact.fact_kind.as_str(),
            fact.title.as_str(),
            fact.path_hint.as_deref().unwrap_or(""),
        );
        if seen.insert(key) {
            selected.push(fact);
        }
    }

    selected.truncate(limit);
    selected
}

fn map_recent_entry(row: &rusqlite::Row<'_>) -> Result<CodexMemoryRecentEntry, rusqlite::Error> {
    let recorded_at_unix: i64 = row.get(1)?;
    Ok(CodexMemoryRecentEntry {
        event_kind: row.get(0)?,
        recorded_at_unix: recorded_at_unix.max(0) as u64,
        target_path: row.get(2)?,
        session_id: row.get(3)?,
        route: row.get(4)?,
        query: row.get(5)?,
        success: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        CodexMemoryFactHit, CodexMemoryStats, QueryProfile, REPO_SYMBOL_INDEX_KIND,
        REPO_SYMBOL_INDEX_VERSION, build_query_profile, entrypoint_body_for_path, event_key,
        extract_symbol_facts, extract_symbol_snippets, fact_score, find_symbol_snippet,
        index_is_fresh, insert_marshaled, map_recent_entry, mark_index_fresh,
        matches_code_extension, open_connection_at, score_code_path, search_code_paths,
        select_render_fact_hits, session_summary_body, upsert_fact,
    };
    use rusqlite::params;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    fn event_key_changes_with_kind_and_payload() {
        let a = event_key("skill_eval_event", "{\"a\":1}");
        let b = event_key("skill_eval_event", "{\"a\":2}");
        let c = event_key("skill_eval_outcome", "{\"a\":1}");
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn inserts_are_deduped_and_recent_rows_roundtrip() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("memory.sqlite");
        let conn = open_connection_at(&db_path).expect("open db");

        let inserted = insert_marshaled(
            &conn,
            "skill_eval_event",
            42,
            Some("/tmp/repo"),
            Some("session-1"),
            Some("runtime-1"),
            Some("new-with-context"),
            Some("write plan"),
            None,
            "{\"route\":\"new-with-context\"}",
        )
        .expect("insert event");
        assert!(inserted);

        let inserted_again = insert_marshaled(
            &conn,
            "skill_eval_event",
            42,
            Some("/tmp/repo"),
            Some("session-1"),
            Some("runtime-1"),
            Some("new-with-context"),
            Some("write plan"),
            None,
            "{\"route\":\"new-with-context\"}",
        )
        .expect("dedupe event");
        assert!(!inserted_again);

        let mut stmt = conn
            .prepare(
                "SELECT event_kind, recorded_at_unix, target_path, session_id, route, query, success \
                 FROM codex_memory_events",
            )
            .expect("prepare select");
        let row = stmt
            .query_row(params![], map_recent_entry)
            .expect("query first row");
        assert_eq!(row.event_kind, "skill_eval_event");
        assert_eq!(row.recorded_at_unix, 42);
        assert_eq!(row.target_path.as_deref(), Some("/tmp/repo"));
        assert_eq!(row.route.as_deref(), Some("new-with-context"));
    }

    #[test]
    fn stats_query_counts_rows() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("memory.sqlite");
        let conn = open_connection_at(&db_path).expect("open db");
        insert_marshaled(
            &conn,
            "skill_eval_event",
            100,
            Some("/tmp/repo"),
            None,
            None,
            Some("route"),
            Some("query"),
            None,
            "{\"kind\":\"event\"}",
        )
        .expect("insert event");
        insert_marshaled(
            &conn,
            "skill_eval_outcome",
            101,
            Some("/tmp/repo"),
            Some("session-1"),
            None,
            None,
            None,
            Some(1.0),
            "{\"kind\":\"outcome\"}",
        )
        .expect("insert outcome");

        let mut stmt = conn
            .prepare(
                "SELECT COUNT(*), \
                        0, \
                        COALESCE(SUM(CASE WHEN event_kind = 'skill_eval_event' THEN 1 ELSE 0 END), 0), \
                        COALESCE(SUM(CASE WHEN event_kind = 'skill_eval_outcome' THEN 1 ELSE 0 END), 0), \
                        MAX(recorded_at_unix) \
                 FROM codex_memory_events",
            )
            .expect("prepare stats");
        let stats: CodexMemoryStats = stmt
            .query_row([], |row| {
                Ok(CodexMemoryStats {
                    root_dir: String::new(),
                    db_path: String::new(),
                    total_events: row.get::<_, i64>(0)? as usize,
                    total_facts: row.get::<_, i64>(1)? as usize,
                    skill_eval_events: row.get::<_, i64>(2)? as usize,
                    skill_eval_outcomes: row.get::<_, i64>(3)? as usize,
                    latest_recorded_at_unix: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                })
            })
            .expect("read stats");
        assert_eq!(stats.total_events, 2);
        assert_eq!(stats.total_facts, 0);
        assert_eq!(stats.skill_eval_events, 1);
        assert_eq!(stats.skill_eval_outcomes, 1);
        assert_eq!(stats.latest_recorded_at_unix, Some(101));
    }

    #[test]
    fn fact_score_prefers_title_and_paths() {
        let profile = build_query_profile("reload speed build123d keyboard");
        let score = fact_score(
            &profile,
            "important_path",
            "Important path: projects/keyboard/keyboard.py",
            "Key file or directory in gumyr/build123d: projects/keyboard/keyboard.py",
            Some("projects/keyboard/keyboard.py"),
        );
        assert!(score > 5.0);
    }

    #[test]
    fn upsert_fact_replaces_existing_row() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("memory.sqlite");
        let conn = open_connection_at(&db_path).expect("open db");
        upsert_fact(
            &conn,
            "/tmp/repo",
            "summary",
            "Summary",
            "first body",
            None,
            "repo_capsule",
            10,
        )
        .expect("insert fact");
        upsert_fact(
            &conn,
            "/tmp/repo",
            "summary",
            "Summary",
            "first body",
            None,
            "repo_capsule",
            20,
        )
        .expect("update fact");
        let updated_at: i64 = conn
            .query_row(
                "SELECT updated_at_unix FROM codex_memory_facts WHERE target_path = '/tmp/repo'",
                [],
                |row| row.get(0),
            )
            .expect("select fact");
        assert_eq!(updated_at, 20);
    }

    #[test]
    fn code_path_search_prefers_matching_files() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join("src")).expect("create src");
        fs::create_dir_all(root.join("docs")).expect("create docs");
        fs::write(root.join("src/codex_runtime.rs"), "// runtime\n").expect("write runtime");
        fs::write(root.join("src/ai.rs"), "// ai\n").expect("write ai");
        fs::write(root.join("docs/runtime-skills.md"), "# Runtime skills\n").expect("write docs");

        let profile = build_query_profile("codex runtime skills");
        let hits = search_code_paths(&root, &profile, 3);
        assert!(!hits.is_empty());
        assert!(hits.iter().any(|hit| hit.path == "src/codex_runtime.rs"));
    }

    #[test]
    fn code_extension_filter_accepts_repo_docs_and_code() {
        assert!(matches_code_extension(Path::new("README.md")));
        assert!(matches_code_extension(Path::new("src/main.rs")));
        assert!(matches_code_extension(Path::new("docs/guide.mdx")));
        assert!(!matches_code_extension(Path::new("target/debug/f")));
    }

    #[test]
    fn code_path_scoring_prefers_exact_filename_hits() {
        let profile = QueryProfile {
            tokens: vec!["codex_runtime".to_string()],
            code_intent: true,
            docs_intent: false,
            explicit_paths: Vec::new(),
        };
        let exact = score_code_path("src/codex_runtime.rs", &profile);
        let loose = score_code_path("src/runtime.rs", &profile);
        assert!(exact > loose);
    }

    #[test]
    fn symbol_extraction_finds_rust_and_ts_entrypoints() {
        let rust = extract_symbol_facts(
            "src/codex_memory.rs",
            "pub fn query_repo_facts() {}\nstruct RepoMemory {}\nmod helpers {}\n",
        );
        assert!(
            rust.iter()
                .any(|fact| fact.title == "Symbol: query_repo_facts")
        );
        assert!(rust.iter().any(|fact| fact.title == "Symbol: RepoMemory"));

        let ts = extract_symbol_facts(
            "src/index.ts",
            "export function startFlow() {}\nexport class CodexBridge {}\nexport const runtimeSkill = 1;\n",
        );
        assert!(ts.iter().any(|fact| fact.title == "Symbol: startFlow"));
        assert!(ts.iter().any(|fact| fact.title == "Symbol: CodexBridge"));
        assert_eq!(
            entrypoint_body_for_path("src/index.ts"),
            Some("Likely runtime entrypoint")
        );
    }

    #[test]
    fn symbol_index_freshness_roundtrips() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("memory.sqlite");
        let conn = open_connection_at(&db_path).expect("open db");

        assert!(
            !index_is_fresh(
                &conn,
                "/tmp/repo",
                REPO_SYMBOL_INDEX_KIND,
                REPO_SYMBOL_INDEX_VERSION,
                42,
            )
            .expect("initial freshness")
        );

        mark_index_fresh(
            &conn,
            "/tmp/repo",
            REPO_SYMBOL_INDEX_KIND,
            REPO_SYMBOL_INDEX_VERSION,
            42,
        )
        .expect("mark fresh");

        assert!(
            index_is_fresh(
                &conn,
                "/tmp/repo",
                REPO_SYMBOL_INDEX_KIND,
                REPO_SYMBOL_INDEX_VERSION,
                42,
            )
            .expect("fresh after mark")
        );
        assert!(
            !index_is_fresh(
                &conn,
                "/tmp/repo",
                REPO_SYMBOL_INDEX_KIND,
                REPO_SYMBOL_INDEX_VERSION,
                43,
            )
            .expect("stale after source change")
        );
    }

    #[test]
    fn render_selection_prefers_symbols_before_doc_noise() {
        let facts = vec![
            CodexMemoryFactHit {
                fact_kind: "doc_heading".to_string(),
                title: "Doc heading: Skills".to_string(),
                body: "Heading in docs/skills.md: Skills".to_string(),
                path_hint: Some("docs/skills.md".to_string()),
                source_tag: "repo_symbols".to_string(),
                score: 10.0,
            },
            CodexMemoryFactHit {
                fact_kind: "symbol".to_string(),
                title: "Symbol: query_repo_facts".to_string(),
                body: "fn query_repo_facts in src/codex_memory.rs".to_string(),
                path_hint: Some("src/codex_memory.rs".to_string()),
                source_tag: "live_symbol".to_string(),
                score: 8.0,
            },
            CodexMemoryFactHit {
                fact_kind: "entrypoint".to_string(),
                title: "Entrypoint: src/main.rs".to_string(),
                body: "Likely runtime entrypoint in repo: src/main.rs".to_string(),
                path_hint: Some("src/main.rs".to_string()),
                source_tag: "repo_symbols".to_string(),
                score: 7.0,
            },
        ];

        let profile = QueryProfile {
            tokens: vec!["implement".to_string()],
            code_intent: true,
            docs_intent: false,
            explicit_paths: Vec::new(),
        };
        let selected = select_render_fact_hits(&facts, &profile, 3);
        assert_eq!(selected[0].fact_kind, "symbol");
        assert_eq!(selected[1].fact_kind, "entrypoint");
    }

    #[test]
    fn query_profile_detects_code_intent_and_explicit_path() {
        let profile = build_query_profile("implement codex runtime skill ranking in src/ai.rs");
        assert!(profile.code_intent);
        assert!(!profile.docs_intent);
        assert!(
            profile
                .explicit_paths
                .iter()
                .any(|value| value == "src/ai.rs")
        );
    }

    #[test]
    fn query_profile_detects_docs_intent() {
        let profile = build_query_profile("summarize codex control plane roadmap");
        assert!(profile.docs_intent);
    }

    #[test]
    fn session_summary_body_strips_contextual_first_prompt_noise() {
        let row = crate::ai::CodexRecoverRow {
            id: "019ce6ce-c77a-7d52-838e-c01f8820f6b8".to_string(),
            updated_at: 42,
            cwd: "/tmp/repo".to_string(),
            title: Some("Session title".to_string()),
            first_user_message: Some(
                "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>\n<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>\nwrite plan for rollout"
                    .to_string(),
            ),
            git_branch: Some("main".to_string()),
        };

        let body = session_summary_body(&row);
        assert!(body.contains("write plan for rollout"));
        assert!(!body.contains("AGENTS.md"));
        assert!(!body.contains("<environment_context>"));
    }

    #[test]
    fn snippet_extraction_returns_compact_symbol_excerpt() {
        let content =
            "pub struct CodexRuntimeSkill {\n    pub name: String,\n    pub path: String,\n}\n";
        let snippet = find_symbol_snippet(content, "CodexRuntimeSkill").expect("snippet");
        assert!(snippet.contains("pub struct CodexRuntimeSkill"));
        assert!(snippet.contains("pub name: String") || snippet.contains("pub name: String,"));
    }

    #[test]
    fn extract_symbol_snippets_picks_top_symbol_hits() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join("src")).expect("create src");
        fs::write(
            root.join("src/codex_runtime.rs"),
            "pub struct CodexRuntimeSkill {\n    pub name: String,\n}\n",
        )
        .expect("write runtime");

        let hits = vec![CodexMemoryFactHit {
            fact_kind: "symbol".to_string(),
            title: "Symbol: CodexRuntimeSkill".to_string(),
            body: "struct CodexRuntimeSkill in src/codex_runtime.rs".to_string(),
            path_hint: Some("src/codex_runtime.rs".to_string()),
            source_tag: "live_symbol".to_string(),
            score: 9.0,
        }];

        let snippets = extract_symbol_snippets(&root, &hits, 2);
        assert_eq!(snippets.len(), 1);
        assert_eq!(snippets[0].path, "src/codex_runtime.rs");
        assert!(snippets[0].snippet.contains("CodexRuntimeSkill"));
    }

    #[test]
    fn render_selection_prefers_session_context_for_general_queries() {
        let facts = vec![
            CodexMemoryFactHit {
                fact_kind: "symbol".to_string(),
                title: "Symbol: CodexRuntimeSkill".to_string(),
                body: "struct CodexRuntimeSkill in src/codex_runtime.rs".to_string(),
                path_hint: Some("src/codex_runtime.rs".to_string()),
                source_tag: "repo_symbols".to_string(),
                score: 8.0,
            },
            CodexMemoryFactHit {
                fact_kind: "session_intent".to_string(),
                title: "Recent Codex intent: runtime work".to_string(),
                body: "implement codex runtime skill ranking".to_string(),
                path_hint: None,
                source_tag: "repo_sessions".to_string(),
                score: 9.0,
            },
            CodexMemoryFactHit {
                fact_kind: "session_exchange".to_string(),
                title: "Recent Codex exchange 1: runtime work".to_string(),
                body: "User: implement codex runtime skill ranking\nAssistant: focus on ai.rs"
                    .to_string(),
                path_hint: None,
                source_tag: "repo_sessions".to_string(),
                score: 8.5,
            },
        ];

        let profile = QueryProfile {
            tokens: vec!["runtime".to_string()],
            code_intent: false,
            docs_intent: false,
            explicit_paths: Vec::new(),
        };
        let selected = select_render_fact_hits(&facts, &profile, 3);
        assert_eq!(selected[0].fact_kind, "session_intent");
        assert_eq!(selected[1].fact_kind, "session_exchange");
    }
}
