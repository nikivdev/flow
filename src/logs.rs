use std::{
    io::{BufRead, BufReader},
    thread,
    time::Duration,
};

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
    let use_color = !opts.no_color;
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("failed to build HTTP client")?;

    if let Some(server) = opts.server.as_deref() {
        if opts.follow {
            stream_server_logs(server, opts.host, opts.port, use_color)?;
        } else {
            let logs = fetch_logs(&client, &base_url, server, opts.limit)?;
            print_logs(&logs, use_color);
        }
        return Ok(());
    }

    match fetch_all_logs(&client, &base_url, opts.limit) {
        Ok(logs) => print_logs(&logs, use_color),
        Err(err) => {
            eprintln!(
                "failed to load aggregated logs: {err:?}\nfallback: fetching per-server logs..."
            );
            let servers = list_servers(&client, &base_url)?;
            for snapshot in servers {
                println!("== {} ==", snapshot.name);
                let logs = fetch_logs(&client, &base_url, &snapshot.name, opts.limit)?;
                print_logs(&logs, use_color);
                println!();
            }
        }
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

fn fetch_all_logs(client: &Client, base: &str, limit: usize) -> Result<Vec<LogLine>> {
    client
        .get(format!("{base}/logs"))
        .query(&[("limit", limit.to_string())])
        .send()
        .context("failed to request aggregated logs")?
        .error_for_status()
        .context("aggregated logs endpoint returned error status")?
        .json::<Vec<LogLine>>()
        .context("failed to decode aggregated logs payload")
}

fn stream_server_logs(server: &str, host: std::net::IpAddr, port: u16, color: bool) -> Result<()> {
    println!("Streaming logs for {server} (Ctrl+C to stop)...");
    let client = Client::builder()
        .timeout(None)
        .build()
        .context("failed to build streaming client")?;

    let url = format!("http://{host}:{port}/servers/{server}/logs/stream");
    let mut backoff = Duration::from_secs(1);

    loop {
        match client.get(&url).send() {
            Ok(response) => match response.error_for_status() {
                Ok(resp) => {
                    backoff = Duration::from_secs(1);
                    let mut reader = BufReader::new(resp);
                    let mut line = String::new();
                    while reader.read_line(&mut line)? != 0 {
                        if let Some(payload) = line.trim().strip_prefix("data:") {
                            let trimmed = payload.trim();
                            if trimmed.is_empty() {
                                line.clear();
                                continue;
                            }
                            match serde_json::from_str::<LogLine>(trimmed) {
                                Ok(entry) => print_log_line(&entry, color),
                                Err(err) => eprintln!("failed to decode log entry: {err:?}"),
                            }
                        }
                        line.clear();
                    }
                    eprintln!("log stream closed, reconnecting...");
                }
                Err(err) => {
                    eprintln!("log stream error: {err}; retrying...");
                }
            },
            Err(err) => {
                eprintln!("failed to connect to log stream: {err:?}");
            }
        }

        thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

fn print_logs(logs: &[LogLine], color: bool) {
    if logs.is_empty() {
        println!("(no logs)\n");
        return;
    }

    for line in logs {
        print_log_line(line, color);
    }
}

fn print_log_line(line: &LogLine, color: bool) {
    let stream = match line.stream {
        LogStream::Stdout => "stdout",
        LogStream::Stderr => "stderr",
    };
    if color {
        match line.stream {
            LogStream::Stdout => {
                println!(
                    "\x1b[38;5;36m[{}][stdout]\x1b[0m {}",
                    line.server,
                    line.line.trim_end()
                );
            }
            LogStream::Stderr => {
                println!("🔴 {}", line.line.trim_end());
            }
        }
    } else {
        println!("[{}][{}] {}", line.server, stream, line.line.trim_end());
    }
}
