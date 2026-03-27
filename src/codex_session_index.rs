use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{ai, config};

const INDEX_SCHEMA_VERSION: i64 = 1;
const INDEX_FETCH_CAP: usize = 48;
const INDEX_FETCH_FACTOR: usize = 6;
const MAX_TRANSCRIPT_SNIPPETS: usize = 8;
const MAX_TRANSCRIPT_CHARS: usize = 2400;
const INDEX_STRONG_RECENT_HIT_SCORE: i64 = 280;

#[derive(Debug, Clone)]
pub(crate) struct CodexSessionIndexHit {
    pub(crate) row: ai::CodexRecoverRow,
    pub(crate) score: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceDbStamp {
    path: String,
    len: u64,
    modified_unix_secs: u64,
}

#[derive(Debug)]
struct IndexedSessionRow {
    row: ai::CodexRecoverRow,
    transcript_excerpt: Option<String>,
}

pub(crate) fn search_codex_sessions(
    target_path: Option<&Path>,
    exact_cwd: bool,
    query: &str,
    limit: usize,
    scope: ai::CodexFindScope,
) -> Result<Vec<CodexSessionIndexHit>> {
    let source_db_path = ai::codex_state_db_path()?;
    let index_db_path = session_index_db_path()?;
    search_codex_sessions_with_paths(
        &source_db_path,
        &index_db_path,
        target_path,
        exact_cwd,
        query,
        limit,
        scope,
    )
}

fn search_codex_sessions_with_paths(
    source_db_path: &Path,
    index_db_path: &Path,
    target_path: Option<&Path>,
    exact_cwd: bool,
    query: &str,
    limit: usize,
    scope: ai::CodexFindScope,
) -> Result<Vec<CodexSessionIndexHit>> {
    let normalized_query = query.trim().to_lowercase();
    let fts_tokens = fts_search_tokens(&normalized_query);
    let rank_tokens = ai::tokenize_recover_query(&normalized_query);
    if normalized_query.is_empty() || fts_tokens.is_empty() {
        return Ok(vec![]);
    }

    let mut conn = open_session_index(index_db_path)?;
    ensure_index_current(&mut conn, source_db_path)?;
    let now_unix = current_unix_secs();
    let recent_cutoff = scope.recent_cutoff_unix(now_unix);
    let mut hits = query_index(
        &conn,
        target_path,
        exact_cwd,
        &normalized_query,
        &fts_tokens,
        &rank_tokens,
        limit.max(1),
        recent_cutoff,
        now_unix,
    )?;

    if recent_cutoff.is_some() && should_expand_index_scope(&hits, limit.max(1)) {
        let expanded_hits = query_index(
            &conn,
            target_path,
            exact_cwd,
            &normalized_query,
            &fts_tokens,
            &rank_tokens,
            limit.max(1),
            None,
            now_unix,
        )?;
        hits = merge_index_hits(hits, expanded_hits, limit.max(1));
    }

    Ok(hits)
}

fn session_index_db_path() -> Result<PathBuf> {
    let root = config::ensure_global_state_dir()?.join("codex");
    fs::create_dir_all(&root).with_context(|| {
        format!(
            "failed to create Codex session index dir {}",
            root.display()
        )
    })?;
    Ok(root.join("session-index.sqlite"))
}

fn open_session_index(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("failed to enable WAL for Codex session index")?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .context("failed to tune Codex session index synchronous mode")?;
    conn.execute_batch(
        r#"
create table if not exists codex_session_index_meta (
    key text primary key,
    value text not null
);

create table if not exists codex_session_index_sessions (
    session_id text primary key,
    updated_at_unix integer not null,
    cwd text not null,
    title text,
    first_user_message text,
    git_branch text,
    model text,
    reasoning_effort text,
    transcript_path text,
    transcript_excerpt text
);

create index if not exists idx_codex_session_index_sessions_cwd_time
    on codex_session_index_sessions(cwd, updated_at_unix desc);

create virtual table if not exists codex_session_index_fts using fts5(
    session_id unindexed,
    title,
    first_user_message,
    git_branch,
    cwd,
    model,
    reasoning_effort,
    transcript_excerpt
);
"#,
    )
    .context("failed to initialize Codex session index schema")?;
    Ok(conn)
}

fn ensure_index_current(conn: &mut Connection, source_db_path: &Path) -> Result<()> {
    let source_stamp = source_db_stamp(source_db_path)?;
    if index_is_current(conn, &source_stamp)? {
        return Ok(());
    }

    rebuild_index(conn, source_db_path, &source_stamp)
}

fn index_is_current(conn: &Connection, source_stamp: &SourceDbStamp) -> Result<bool> {
    let schema_version = meta_value(conn, "schema_version")?;
    if schema_version.as_deref() != Some(INDEX_SCHEMA_VERSION.to_string().as_str()) {
        return Ok(false);
    }

    let Some(stored_path) = meta_value(conn, "source_db_path")? else {
        return Ok(false);
    };
    let Some(stored_len) = meta_value(conn, "source_db_len")? else {
        return Ok(false);
    };
    let Some(stored_mtime) = meta_value(conn, "source_db_mtime")? else {
        return Ok(false);
    };
    let count: i64 = conn
        .query_row(
            "select count(*) from codex_session_index_sessions",
            [],
            |row| row.get(0),
        )
        .context("failed to read Codex session index row count")?;

    Ok(stored_path == source_stamp.path
        && stored_len == source_stamp.len.to_string()
        && stored_mtime == source_stamp.modified_unix_secs.to_string()
        && count >= 0)
}

fn rebuild_index(
    conn: &mut Connection,
    source_db_path: &Path,
    source_stamp: &SourceDbStamp,
) -> Result<()> {
    let source_conn = Connection::open(source_db_path)
        .with_context(|| format!("failed to open {}", source_db_path.display()))?;
    let schema = ai::load_codex_thread_schema(source_db_path)?;
    let sql = format!(
        r#"
{}
where archived = 0
order by updated_at desc
"#,
        ai::codex_recover_select_sql(&schema)
    );

    let rows = {
        let mut stmt = source_conn
            .prepare(&sql)
            .context("failed to prepare Codex session index source query")?;
        let iter = stmt.query_map([], ai::map_codex_recover_row)?;
        iter.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to load Codex sessions for session index")?
    };

    let indexed_rows = rows
        .into_iter()
        .map(|row| {
            let transcript_excerpt = ai::read_codex_session_search_excerpt(
                &row,
                MAX_TRANSCRIPT_SNIPPETS,
                MAX_TRANSCRIPT_CHARS,
            )?;
            Ok(IndexedSessionRow {
                row,
                transcript_excerpt,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let tx = conn
        .transaction()
        .context("failed to start Codex session index transaction")?;
    tx.execute("delete from codex_session_index_fts", [])
        .context("failed to clear Codex session index FTS rows")?;
    tx.execute("delete from codex_session_index_sessions", [])
        .context("failed to clear Codex session index session rows")?;

    for entry in indexed_rows {
        let transcript_path = entry.row.rollout_path.clone();
        tx.execute(
            "insert into codex_session_index_sessions (
                session_id,
                updated_at_unix,
                cwd,
                title,
                first_user_message,
                git_branch,
                model,
                reasoning_effort,
                transcript_path,
                transcript_excerpt
            ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                entry.row.id,
                entry.row.updated_at,
                entry.row.cwd,
                entry.row.title,
                entry.row.first_user_message,
                entry.row.git_branch,
                entry.row.model,
                entry.row.reasoning_effort,
                transcript_path,
                entry.transcript_excerpt,
            ],
        )
        .context("failed to insert Codex session index row")?;

        tx.execute(
            "insert into codex_session_index_fts (
                session_id,
                title,
                first_user_message,
                git_branch,
                cwd,
                model,
                reasoning_effort,
                transcript_excerpt
            ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                entry.row.id,
                entry.row.title,
                entry.row.first_user_message,
                entry.row.git_branch,
                entry.row.cwd,
                entry.row.model,
                entry.row.reasoning_effort,
                entry.transcript_excerpt,
            ],
        )
        .context("failed to insert Codex session index FTS row")?;
    }

    upsert_meta(&tx, "schema_version", &INDEX_SCHEMA_VERSION.to_string())?;
    upsert_meta(&tx, "source_db_path", &source_stamp.path)?;
    upsert_meta(&tx, "source_db_len", &source_stamp.len.to_string())?;
    upsert_meta(
        &tx,
        "source_db_mtime",
        &source_stamp.modified_unix_secs.to_string(),
    )?;
    tx.commit()
        .context("failed to commit Codex session index rebuild")?;
    Ok(())
}

fn query_index(
    conn: &Connection,
    target_path: Option<&Path>,
    exact_cwd: bool,
    normalized_query: &str,
    fts_tokens: &[String],
    rank_tokens: &[String],
    limit: usize,
    recent_cutoff_unix: Option<i64>,
    now_unix: i64,
) -> Result<Vec<CodexSessionIndexHit>> {
    let fetch_limit = (limit.max(1) * INDEX_FETCH_FACTOR).clamp(12, INDEX_FETCH_CAP);
    let fts_query = build_fts_query(fts_tokens);
    if fts_query.is_empty() {
        return Ok(vec![]);
    }

    let mut sql = r#"
select
    s.session_id,
    s.updated_at_unix,
    s.cwd,
    s.title,
    s.first_user_message,
    s.git_branch,
    s.model,
    s.reasoning_effort,
    s.transcript_path,
    s.transcript_excerpt,
    bm25(
        codex_session_index_fts,
        8.0,
        9.0,
        4.0,
        2.0,
        1.0,
        0.5,
        4.0
    ) as fts_rank
from codex_session_index_fts
join codex_session_index_sessions s
  on s.session_id = codex_session_index_fts.session_id
where codex_session_index_fts match ?
"#
    .to_string();
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(fts_query)];

    if let Some(target_path) = target_path {
        let target = target_path.to_string_lossy().to_string();
        if exact_cwd {
            sql.push_str("  and s.cwd = ?\n");
            params_vec.push(Box::new(target));
        } else {
            sql.push_str("  and (s.cwd = ? or s.cwd like ? escape '\\')\n");
            params_vec.push(Box::new(target.clone()));
            params_vec.push(Box::new(format!("{}/%", escape_like(&target))));
        }
    }

    if let Some(recent_cutoff_unix) = recent_cutoff_unix {
        sql.push_str("  and s.updated_at_unix >= ?\n");
        params_vec.push(Box::new(recent_cutoff_unix));
    }

    sql.push_str("order by fts_rank asc, s.updated_at_unix desc\nlimit ?\n");
    params_vec.push(Box::new(fetch_limit as i64));

    let mut stmt = conn
        .prepare(&sql)
        .context("failed to prepare Codex session index query")?;
    let params_refs: Vec<&dyn rusqlite::ToSql> =
        params_vec.iter().map(|value| value.as_ref()).collect();
    let rows = stmt
        .query_map(params_refs.as_slice(), |row| {
            Ok(IndexedSessionRow {
                row: ai::CodexRecoverRow {
                    id: row.get("session_id")?,
                    rollout_path: row.get("transcript_path")?,
                    updated_at: row.get("updated_at_unix")?,
                    cwd: row.get("cwd")?,
                    title: row.get("title")?,
                    first_user_message: row.get("first_user_message")?,
                    git_branch: row.get("git_branch")?,
                    model: row.get("model")?,
                    reasoning_effort: row.get("reasoning_effort")?,
                },
                transcript_excerpt: row.get("transcript_excerpt")?,
            })
        })
        .context("failed to execute Codex session index query")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to collect Codex session index hits")?;

    let mut hits = rows
        .into_iter()
        .enumerate()
        .map(|(index, entry)| CodexSessionIndexHit {
            score: compute_hit_score(
                &entry,
                target_path,
                exact_cwd,
                normalized_query,
                rank_tokens,
                index,
                fetch_limit,
                now_unix,
            ),
            row: entry.row,
        })
        .collect::<Vec<_>>();
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.row.updated_at.cmp(&a.row.updated_at))
            .then_with(|| a.row.cwd.cmp(&b.row.cwd))
    });
    hits.truncate(limit.max(1));
    Ok(hits)
}

fn compute_hit_score(
    entry: &IndexedSessionRow,
    target_path: Option<&Path>,
    exact_cwd: bool,
    normalized_query: &str,
    tokens: &[String],
    rank_index: usize,
    fetch_limit: usize,
    now_unix: i64,
) -> i64 {
    let mut score = ai::recover_row_score(&entry.row, normalized_query, tokens) * 12
        + ai::codex_find_recency_bonus(entry.row.updated_at, now_unix) * 4
        + ai::codex_find_path_affinity(&entry.row.cwd, target_path, exact_cwd) * 6
        + (fetch_limit.saturating_sub(rank_index)) as i64;
    let transcript = entry
        .transcript_excerpt
        .clone()
        .unwrap_or_default()
        .to_lowercase();

    if transcript.contains(normalized_query) {
        score += 240;
    }

    for token in tokens {
        let structured = ai::is_structured_find_token(token);
        if token.len() <= 1 {
            continue;
        }
        if transcript.contains(token) {
            score += if structured { 36 } else { 18 };
        }
    }

    score
}

fn should_expand_index_scope(hits: &[CodexSessionIndexHit], limit: usize) -> bool {
    hits.len() < limit
        || hits
            .first()
            .map(|hit| hit.score < INDEX_STRONG_RECENT_HIT_SCORE)
            .unwrap_or(true)
}

fn merge_index_hits(
    hits: Vec<CodexSessionIndexHit>,
    expanded_hits: Vec<CodexSessionIndexHit>,
    limit: usize,
) -> Vec<CodexSessionIndexHit> {
    let mut merged = std::collections::BTreeMap::<String, CodexSessionIndexHit>::new();
    for hit in hits.into_iter().chain(expanded_hits.into_iter()) {
        merged
            .entry(hit.row.id.clone())
            .and_modify(|existing| {
                if hit.score > existing.score
                    || (hit.score == existing.score && hit.row.updated_at > existing.row.updated_at)
                {
                    *existing = hit.clone();
                }
            })
            .or_insert(hit);
    }

    let mut values = merged.into_values().collect::<Vec<_>>();
    values.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.row.updated_at.cmp(&a.row.updated_at))
            .then_with(|| a.row.cwd.cmp(&b.row.cwd))
    });
    values.truncate(limit);
    values
}

fn build_fts_query(tokens: &[String]) -> String {
    tokens
        .iter()
        .filter(|token| !token.is_empty())
        .map(|token| {
            if token.len() >= 3 {
                format!("{token}*")
            } else {
                token.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn fts_search_tokens(query: &str) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut tokens = Vec::new();
    for token in query.split(|ch: char| !ch.is_ascii_alphanumeric()) {
        let normalized = token.trim().to_lowercase();
        if normalized.len() <= 1 {
            continue;
        }
        if seen.insert(normalized.clone()) {
            tokens.push(normalized);
        }
    }
    tokens
}

fn meta_value(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "select value from codex_session_index_meta where key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .context("failed to read Codex session index metadata")
}

fn upsert_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "insert into codex_session_index_meta(key, value)
         values (?1, ?2)
         on conflict(key) do update set value = excluded.value",
        params![key, value],
    )
    .context("failed to write Codex session index metadata")?;
    Ok(())
}

fn current_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn source_db_stamp(path: &Path) -> Result<SourceDbStamp> {
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    let modified_unix_secs = metadata
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(SourceDbStamp {
        path: path.display().to_string(),
        len: metadata.len(),
        modified_unix_secs,
    })
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn indexed_search_finds_transcript_only_phrase() -> Result<()> {
        let temp = tempdir()?;
        let source_db_path = temp.path().join("state_1.sqlite");
        let index_db_path = temp.path().join("session-index.sqlite");
        let session_file = temp.path().join("session-1.jsonl");

        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-24T15:37:50Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"explain rust analyzer server design\"}]}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2026-03-24T15:37:54Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"keep lsp json conversion at the boundary and maintain one mutable server state\"}]}}\n"
            ),
        )?;

        let source_conn = Connection::open(&source_db_path)?;
        source_conn.execute_batch(
            r#"
create table threads (
    id text primary key,
    rollout_path text,
    updated_at integer not null,
    cwd text not null,
    title text,
    first_user_message text,
    git_branch text,
    model text,
    reasoning_effort text,
    archived integer not null default 0
);
"#,
        )?;
        source_conn.execute(
            "insert into threads (
                id,
                rollout_path,
                updated_at,
                cwd,
                title,
                first_user_message,
                git_branch,
                model,
                reasoning_effort,
                archived
            ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0)",
            params![
                "019d0000-0000-7000-8000-aaaaaaaaaaaa",
                session_file.display().to_string(),
                1_774_830_000i64,
                "/tmp/repo",
                "rust analyzer session",
                "explain rust analyzer server design",
                "main",
                "gpt-5.4",
                "medium",
            ],
        )?;

        let hits = search_codex_sessions_with_paths(
            &source_db_path,
            &index_db_path,
            Some(Path::new("/tmp/repo")),
            true,
            "keep lsp json conversion",
            3,
            ai::CodexFindScope::default(),
        )?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].row.id, "019d0000-0000-7000-8000-aaaaaaaaaaaa");
        assert!(hits[0].score > 0);
        Ok(())
    }

    #[test]
    fn indexed_search_expands_beyond_recent_window_when_needed() -> Result<()> {
        let temp = tempdir()?;
        let source_db_path = temp.path().join("state_1.sqlite");
        let index_db_path = temp.path().join("session-index.sqlite");
        let session_file = temp.path().join("session-old.jsonl");

        fs::write(
            &session_file,
            concat!(
                "{\"type\":\"response_item\",\"timestamp\":\"2025-02-24T15:37:50Z\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"explain calibration ladder search\"}]}}\n",
                "{\"type\":\"response_item\",\"timestamp\":\"2025-02-24T15:37:54Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"the calibration ladder fallback should widen scope when recent history is sparse\"}]}}\n"
            ),
        )?;

        let source_conn = Connection::open(&source_db_path)?;
        source_conn.execute_batch(
            r#"
create table threads (
    id text primary key,
    rollout_path text,
    updated_at integer not null,
    cwd text not null,
    title text,
    first_user_message text,
    git_branch text,
    model text,
    reasoning_effort text,
    archived integer not null default 0
);
"#,
        )?;
        source_conn.execute(
            "insert into threads (
                id,
                rollout_path,
                updated_at,
                cwd,
                title,
                first_user_message,
                git_branch,
                model,
                reasoning_effort,
                archived
            ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0)",
            params![
                "019d0000-0000-7000-8000-bbbbbbbbbbbb",
                session_file.display().to_string(),
                1_740_000_000i64,
                "/tmp/repo",
                "calibration ladder search",
                "explain calibration ladder search",
                "main",
                "gpt-5.4",
                "medium",
            ],
        )?;

        let hits = search_codex_sessions_with_paths(
            &source_db_path,
            &index_db_path,
            Some(Path::new("/tmp/repo")),
            true,
            "calibration ladder",
            1,
            ai::CodexFindScope::default(),
        )?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].row.id, "019d0000-0000-7000-8000-bbbbbbbbbbbb");
        Ok(())
    }

    #[test]
    fn indexed_search_prefers_exact_target_path_over_descendant() -> Result<()> {
        let temp = tempdir()?;
        let source_db_path = temp.path().join("state_1.sqlite");
        let index_db_path = temp.path().join("session-index.sqlite");

        let source_conn = Connection::open(&source_db_path)?;
        source_conn.execute_batch(
            r#"
create table threads (
    id text primary key,
    rollout_path text,
    updated_at integer not null,
    cwd text not null,
    title text,
    first_user_message text,
    git_branch text,
    model text,
    reasoning_effort text,
    archived integer not null default 0
);
"#,
        )?;
        source_conn.execute(
            "insert into threads (
                id,
                rollout_path,
                updated_at,
                cwd,
                title,
                first_user_message,
                git_branch,
                model,
                reasoning_effort,
                archived
            ) values (?1, null, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
            params![
                "019d0000-0000-7000-8000-rootrootroot",
                1_774_480_000i64,
                "/tmp/run",
                "ci/cd designer",
                "plan ci/cd designer rollout",
                "main",
                "gpt-5.4",
                "medium",
            ],
        )?;
        source_conn.execute(
            "insert into threads (
                id,
                rollout_path,
                updated_at,
                cwd,
                title,
                first_user_message,
                git_branch,
                model,
                reasoning_effort,
                archived
            ) values (?1, null, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
            params![
                "019d0000-0000-7000-8000-childchildc",
                1_774_560_000i64,
                "/tmp/run/ide/designer",
                "ci/cd designer",
                "plan ci/cd designer rollout",
                "main",
                "gpt-5.4",
                "medium",
            ],
        )?;

        let hits = search_codex_sessions_with_paths(
            &source_db_path,
            &index_db_path,
            Some(Path::new("/tmp/run")),
            false,
            "ci/cd designer",
            2,
            ai::CodexFindScope {
                recent_days: None,
                all_history: true,
            },
        )?;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].row.id, "019d0000-0000-7000-8000-rootrootroot");
        Ok(())
    }

    #[test]
    fn indexed_search_prefers_structured_query_token_match() -> Result<()> {
        let temp = tempdir()?;
        let source_db_path = temp.path().join("state_1.sqlite");
        let index_db_path = temp.path().join("session-index.sqlite");

        let source_conn = Connection::open(&source_db_path)?;
        source_conn.execute_batch(
            r#"
create table threads (
    id text primary key,
    rollout_path text,
    updated_at integer not null,
    cwd text not null,
    title text,
    first_user_message text,
    git_branch text,
    model text,
    reasoning_effort text,
    archived integer not null default 0
);
"#,
        )?;
        source_conn.execute(
            "insert into threads (
                id,
                rollout_path,
                updated_at,
                cwd,
                title,
                first_user_message,
                git_branch,
                model,
                reasoning_effort,
                archived
            ) values (?1, null, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
            params![
                "019d0000-0000-7000-8000-cicdcicdcicd",
                1_774_000_000i64,
                "/tmp/run",
                "ci/cd rollout",
                "ci/cd in both mac mini and github action mac minis",
                "main",
                "gpt-5.4",
                "medium",
            ],
        )?;
        source_conn.execute(
            "insert into threads (
                id,
                rollout_path,
                updated_at,
                cwd,
                title,
                first_user_message,
                git_branch,
                model,
                reasoning_effort,
                archived
            ) values (?1, null, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
            params![
                "019d0000-0000-7000-8000-designmatch",
                1_774_560_000i64,
                "/tmp/run",
                "designer agent inventory",
                "designer agent summary for run",
                "main",
                "gpt-5.4",
                "medium",
            ],
        )?;

        let hits = search_codex_sessions_with_paths(
            &source_db_path,
            &index_db_path,
            Some(Path::new("/tmp/run")),
            false,
            "ci/cd designer",
            2,
            ai::CodexFindScope {
                recent_days: None,
                all_history: true,
            },
        )?;
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].row.id, "019d0000-0000-7000-8000-cicdcicdcicd");
        Ok(())
    }
}
