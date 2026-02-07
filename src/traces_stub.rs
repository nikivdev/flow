use anyhow::{Result, bail};

use crate::cli::{TraceSessionOpts, TraceSource, TracesOpts};
use crate::base_tool;

pub fn run(opts: TracesOpts) -> Result<()> {
    let Some(bin) = base_tool::resolve_bin() else {
        bail!(
            "traces require the base tool (FLOW_BASE_BIN).\n\
             Install it, then retry.\n\
             (Expected `base` or `db` on PATH, or set FLOW_BASE_BIN=/path/to/base)"
        );
    };

    let mut args: Vec<String> = vec!["trace".to_string(), "--limit".to_string(), opts.limit.to_string()];
    if opts.follow {
        args.push("--follow".to_string());
    }
    if let Some(project) = opts.project.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        args.push("--project".to_string());
        args.push(project.to_string());
    }
    args.push("--source".to_string());
    args.push(match opts.source {
        TraceSource::All => "all",
        TraceSource::Tasks => "tasks",
        TraceSource::Ai => "ai",
    }.to_string());

    base_tool::run_inherit_stdio(&bin, &args)
}

pub fn run_session(_opts: TraceSessionOpts) -> Result<()> {
    let Some(bin) = base_tool::resolve_bin() else {
        bail!(
            "trace session requires the base tool (FLOW_BASE_BIN).\n\
             Install it, then retry.\n\
             (Expected `base` or `db` on PATH, or set FLOW_BASE_BIN=/path/to/base)"
        );
    };

    // Keep behavior compatible with Flow's old implementation: always show full session history.
    let mut args: Vec<String> = vec!["session".to_string()];
    args.push(_opts.path.display().to_string());
    base_tool::run_inherit_stdio(&bin, &args)
}

pub fn trace_source_from_str(_value: &str) -> TraceSource {
    TraceSource::Tasks
}
