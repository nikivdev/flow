use std::io::{BufRead, BufReader};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;

use crate::{
    cli::LogsOpts,
    servers::{LogLine, LogStream, ServerSnapshot},
};

pub fn run(opts: LogsOpts) -> Result<()> {
    if opts.follow && opts.server.is_none() {
        bail!("--follow requires specifying --server <name>");
    }

    let base_url = format!("http://{}:{}", opts.host, opts.port);
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("failed to build HTTP client")?;

    if let Some(server) = opts.server.as_deref() {
        if opts.follow {
            stream_server_logs(&client, &base_url, server)?;
        } else {
            let logs = fetch_logs(&client, &base_url, server, opts.limit)?;
            print_logs(server, &logs);
        }
        return Ok(());
    }

    let servers = list_servers(&client, &base_url)?;
    for snapshot in servers {
        println!("== {} ==", snapshot.name);
        let logs = fetch_logs(&client, &base_url, &snapshot.name, opts.limit)?;
        print_logs(&snapshot.name, &logs);
        println!();
    }

    Ok(())
}

fn list_servers(client: &Client, base: &str) -> Result<Vec<ServerSnapshot>> {
    client
        .get(format!("{base}/servers"))
        .send()
        .context("failed to fetch server list")?
        .error_for_status()
        .context("server list returned non-success status")?
        .json::<Vec<ServerSnapshot>>()
        .context("failed to decode server list json")
}

fn fetch_logs(client: &Client, base: &str, server: &str, limit: usize) -> Result<Vec<LogLine>> {
    client
        .get(format!("{base}/servers/{server}/logs"))
        .query(&[("limit", limit.to_string())])
        .send()
        .with_context(|| format!("failed to request logs for {server}"))?
        .error_for_status()
        .with_context(|| format!("server {server} returned error status"))?
        .json::<Vec<LogLine>>()
        .with_context(|| format!("failed to decode log payload for {server}"))
}

fn stream_server_logs(client: &Client, base: &str, server: &str) -> Result<()> {
    println!("Streaming logs for {server} (Ctrl+C to stop)...");
    let response = client
        .get(format!("{base}/servers/{server}/logs/stream"))
        .send()
        .with_context(|| format!("failed to stream logs for {server}"))?
        .error_for_status()
        .with_context(|| format!("log stream request for {server} failed"))?;

    let mut reader = BufReader::new(response);
    let mut line = String::new();
    while reader.read_line(&mut line)? != 0 {
        if let Some(payload) = line.trim().strip_prefix("data:") {
            let trimmed = payload.trim();
            if trimmed.is_empty() {
                line.clear();
                continue;
            }
            match serde_json::from_str::<LogLine>(trimmed) {
                Ok(entry) => print_log_line(&entry),
                Err(err) => {
                    eprintln!("failed to decode log entry: {err:?}");
                }
            }
        }
        line.clear();
    }

    Ok(())
}

fn print_logs(_server: &str, logs: &[LogLine]) {
    if logs.is_empty() {
        println!("(no logs)\n");
        return;
    }

    for line in logs {
        print_log_line(line);
    }
}

fn print_log_line(line: &LogLine) {
    let stream = match line.stream {
        LogStream::Stdout => "stdout",
        LogStream::Stderr => "stderr",
    };
    println!("[{}][{}] {}", line.server, stream, line.line.trim_end());
}
