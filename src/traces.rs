use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use groove::ObjectId;
use groove::sql::{Database, RowValue};
use groove_rocksdb::RocksEnvironment;

use crate::cli::{TraceSource, TracesOpts};
use crate::jazz_state;

const DEFAULT_LIMIT: usize = 40;
const DETAIL_LIMIT: usize = 120;
const FOLLOW_POLL_MS: u64 = 400;

pub fn run(opts: TracesOpts) -> Result<()> {
    let flow_path = jazz_state::state_dir();
    let flow_db = open_db_at(&flow_path)?;
    let ai_db = open_ai_db(&flow_path);
    let limit = if opts.limit == 0 {
        DEFAULT_LIMIT
    } else {
        opts.limit
    };

    if opts.follow {
        follow_traces(&flow_db, ai_db.as_ref(), &opts, limit)
    } else {
        let mut items = fetch_all(&flow_db, ai_db.as_ref(), &opts, 0, limit, false)?;
        items.sort_by_key(|item| item.timestamp_ms);
        for item in items {
            println!("{}", format_item(&item));
        }
        Ok(())
    }
}

fn follow_traces(
    flow_db: &Database,
    ai_db: Option<&Database>,
    opts: &TracesOpts,
    limit: usize,
) -> Result<()> {
    let mut since = 0u64;
    let mut initial = fetch_all(flow_db, ai_db, opts, 0, limit, true)?;
    initial.sort_by_key(|item| item.timestamp_ms);
    for item in &initial {
        println!("{}", format_item(item));
        since = since.max(item.timestamp_ms);
    }

    loop {
        std::thread::sleep(Duration::from_millis(FOLLOW_POLL_MS));
        let mut items = fetch_all(flow_db, ai_db, opts, since, limit, true)?;
        items.sort_by_key(|item| item.timestamp_ms);
        for item in &items {
            if item.timestamp_ms > since {
                println!("{}", format_item(item));
                since = since.max(item.timestamp_ms);
            }
        }
    }
}

fn fetch_all(
    flow_db: &Database,
    ai_db: Option<&Database>,
    opts: &TracesOpts,
    since: u64,
    limit: usize,
    ascending: bool,
) -> Result<Vec<TraceItem>> {
    let mut items = Vec::new();
    match opts.source {
        TraceSource::All => {
            items.extend(fetch_task_runs(
                flow_db,
                opts.project.as_deref(),
                since,
                limit,
                ascending,
            )?);
            let agent_db = ai_db.unwrap_or(flow_db);
            items.extend(fetch_agent_events(
                agent_db,
                opts.project.as_deref(),
                since,
                limit,
                ascending,
            )?);
        }
        TraceSource::Tasks => {
            items.extend(fetch_task_runs(
                flow_db,
                opts.project.as_deref(),
                since,
                limit,
                ascending,
            )?);
        }
        TraceSource::Ai => {
            let agent_db = ai_db.unwrap_or(flow_db);
            items.extend(fetch_agent_events(
                agent_db,
                opts.project.as_deref(),
                since,
                limit,
                ascending,
            )?);
        }
    }
    Ok(items)
}

fn fetch_task_runs(
    db: &Database,
    project_filter: Option<&str>,
    since: u64,
    limit: usize,
    ascending: bool,
) -> Result<Vec<TraceItem>> {
    let order = if ascending { "ASC" } else { "DESC" };
    let sql = format!(
        "SELECT task, command, success, status, duration_ms, timestamp_ms, output, project_root \
         FROM flow_task_runs WHERE timestamp_ms > {} ORDER BY timestamp_ms {} LIMIT {}",
        since, order, limit
    );

    let rows = db.query(&sql).unwrap_or_default();
    let mut items = Vec::new();
    for (_, row) in rows {
        let task = match row.get_by_name("task") {
            Some(RowValue::String(s)) => s.to_string(),
            _ => continue,
        };
        let project = match row.get_by_name("project_root") {
            Some(RowValue::String(s)) => s.to_string(),
            _ => String::new(),
        };
        if let Some(filter) = project_filter {
            if !project.contains(filter) {
                continue;
            }
        }
        let success = matches!(row.get_by_name("success"), Some(RowValue::Bool(true)));
        let timestamp_ms = match row.get_by_name("timestamp_ms") {
            Some(RowValue::I64(ts)) => ts as u64,
            _ => 0,
        };
        let duration_ms = match row.get_by_name("duration_ms") {
            Some(RowValue::I64(d)) => d as u64,
            _ => 0,
        };
        let status = match row.get_by_name("status") {
            Some(RowValue::I64(s)) => Some(s),
            _ => None,
        };
        let output = match row.get_by_name("output") {
            Some(RowValue::String(s)) => s.to_string(),
            _ => String::new(),
        };
        let command = match row.get_by_name("command") {
            Some(RowValue::String(s)) => s.to_string(),
            _ => String::new(),
        };

        let kind = if success { "task_ok" } else { "task_fail" };
        let detail = if success {
            format!("{}ms", duration_ms)
        } else {
            let status_str = status.map(|s| format!("exit {}", s)).unwrap_or_default();
            let last_line = output.lines().last().unwrap_or("").trim();
            if last_line.is_empty() {
                status_str
            } else {
                format!("{} | {}", status_str, last_line)
            }
        };

        items.push(TraceItem {
            timestamp_ms,
            source: "task",
            kind: kind.to_string(),
            summary: task,
            detail,
            project,
            extra: command,
        });
    }
    Ok(items)
}

fn fetch_agent_events(
    db: &Database,
    project_filter: Option<&str>,
    since: u64,
    limit: usize,
    ascending: bool,
) -> Result<Vec<TraceItem>> {
    let order = if ascending { "ASC" } else { "DESC" };
    let sql = format!(
        "SELECT event_kind, summary, detail, timestamp_ms, project_root \
         FROM ai_agent_events WHERE timestamp_ms > {} ORDER BY timestamp_ms {} LIMIT {}",
        since, order, limit
    );

    let rows = db.query(&sql).unwrap_or_default();
    let mut items = Vec::new();
    for (_, row) in rows {
        let kind = match row.get_by_name("event_kind") {
            Some(RowValue::String(s)) => s.to_string(),
            _ => continue,
        };
        let summary = match row.get_by_name("summary") {
            Some(RowValue::String(s)) => s.to_string(),
            _ => String::new(),
        };
        let detail = match row.get_by_name("detail") {
            Some(RowValue::String(s)) => s.to_string(),
            _ => String::new(),
        };
        let timestamp_ms = match row.get_by_name("timestamp_ms") {
            Some(RowValue::I64(ts)) => ts as u64,
            _ => 0,
        };
        let project = match row.get_by_name("project_root") {
            Some(RowValue::String(s)) => s.to_string(),
            _ => String::new(),
        };
        if let Some(filter) = project_filter {
            if !project.contains(filter) {
                continue;
            }
        }

        items.push(TraceItem {
            timestamp_ms,
            source: "ai",
            kind,
            summary,
            detail,
            project,
            extra: String::new(),
        });
    }
    Ok(items)
}

#[derive(Clone)]
struct TraceItem {
    timestamp_ms: u64,
    source: &'static str,
    kind: String,
    summary: String,
    detail: String,
    project: String,
    extra: String,
}

fn format_item(item: &TraceItem) -> String {
    let time = format_timestamp(item.timestamp_ms);
    let project = project_label(&item.project);
    let detail = truncate(&item.detail, DETAIL_LIMIT);
    let summary = if item.summary.is_empty() {
        item.kind.clone()
    } else {
        item.summary.clone()
    };
    let extra = if item.extra.is_empty() {
        String::new()
    } else {
        format!(" | {}", truncate(&item.extra, 60))
    };

    format!(
        "{} {:>4} {:<10} {} ({}) | {}{}",
        time, item.source, item.kind, summary, project, detail, extra
    )
}

fn format_timestamp(timestamp_ms: u64) -> String {
    let system_time = UNIX_EPOCH + Duration::from_millis(timestamp_ms);
    let dt: chrono::DateTime<chrono::Local> = system_time.into();
    dt.format("%H:%M:%S%.3f").to_string()
}

fn project_label(project: &str) -> String {
    project.rsplit('/').next().unwrap_or(project).to_string()
}

fn truncate(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn open_db_at(path: &Path) -> Result<Database> {
    use groove::Environment;

    if !path.exists() {
        bail!("jazz2 state not found at {}", path.display());
    }

    let env: Arc<dyn Environment> =
        Arc::new(RocksEnvironment::open_readonly(&path).context("open rocksdb")?);
    let catalog_id = load_catalog_id(&path).context("load catalog id")?;
    let db = futures::executor::block_on(Database::from_env(env, catalog_id))
        .context("load jazz2 catalog")?;
    Ok(db)
}

fn open_ai_db(flow_path: &Path) -> Option<Database> {
    let path = if let Ok(path) = env::var("AI_JAZZ2_PATH") {
        PathBuf::from(path)
    } else {
        flow_path.join("ai")
    };
    if path == flow_path || !path.exists() {
        return None;
    }
    open_db_at(&path).ok()
}

fn load_catalog_id(base: &Path) -> Result<ObjectId> {
    let path = base.join("catalog.id");
    let contents =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let trimmed = contents.trim();
    let id = trimmed
        .parse::<ObjectId>()
        .with_context(|| format!("parse catalog id {}", trimmed))?;
    Ok(id)
}
