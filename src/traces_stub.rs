use anyhow::Result;

use crate::cli::{TraceSessionOpts, TraceSource, TracesOpts};

pub fn run(_opts: TracesOpts) -> Result<()> {
    println!("traces disabled (flow built without jazz2 support)");
    Ok(())
}

pub fn run_session(_opts: TraceSessionOpts) -> Result<()> {
    println!("traces disabled (flow built without jazz2 support)");
    Ok(())
}

pub fn trace_source_from_str(_value: &str) -> TraceSource {
    TraceSource::Tasks
}
