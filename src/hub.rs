use std::{
    net::IpAddr,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::{cli::HubOpts, config};

pub fn run(opts: HubOpts) -> Result<()> {
    let host = opts.host;
    let port = opts.port;
    let config_path = opts.config.unwrap_or_else(config::default_config_path);

    if is_daemon_alive(host, port)? {
        println!("hub already running at http://{host}:{port}");
        return Ok(());
    }

    println!(
        "Starting hub daemon at http://{host}:{port} using config {}",
        config_path.display()
    );
    start_daemon(host, port, &config_path)?;
    wait_for_daemon(host, port)?;
    println!("hub ready at http://{host}:{port}");
    Ok(())
}

fn is_daemon_alive(host: IpAddr, port: u16) -> Result<bool> {
    let url = format!("http://{host}:{port}/health");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .context("failed to build HTTP client")?;

    match client.get(url).send() {
        Ok(resp) => Ok(resp.status().is_success()),
        Err(_err) => Ok(false),
    }
}

fn start_daemon(host: IpAddr, port: u16, config_path: &PathBuf) -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .arg("--host")
        .arg(host.to_string())
        .arg("--port")
        .arg(port.to_string());

    if !config_path.as_os_str().is_empty() {
        cmd.arg("--config").arg(config_path);
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn hub daemon")?;
    Ok(())
}

fn wait_for_daemon(host: IpAddr, port: u16) -> Result<()> {
    const MAX_ATTEMPTS: u8 = 10;
    const BACKOFF: Duration = Duration::from_millis(300);

    for _ in 0..MAX_ATTEMPTS {
        if is_daemon_alive(host, port)? {
            return Ok(());
        }
        thread::sleep(BACKOFF);
    }

    let total_ms = BACKOFF.as_millis() as u64 * MAX_ATTEMPTS as u64;
    let total_secs = total_ms as f64 / 1000.0;
    bail!(
        "daemon did not become ready at http://{host}:{port} within {:.1}s",
        total_secs
    );
}
