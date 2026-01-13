use std::{
    net::IpAddr,
    time::Duration,
};

use anyhow::Result;
use reqwest::blocking::Client;

use crate::{
    cli::{HubAction, HubCommand, HubOpts},
    daemon, docs, supervisor,
};

/// Flow acts as a thin launcher that makes sure the lin hub daemon is running.
pub fn run(cmd: HubCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(HubAction::Start);
    let opts = cmd.opts;

    match action {
        HubAction::Start => {
            ensure_daemon(opts)?;
            let docs_opts = crate::cli::DocsHubOpts {
                host: "127.0.0.1".to_string(),
                port: 4410,
                hub_root: "~/.config/flow/docs-hub".to_string(),
                template_root: "~/new/docs".to_string(),
                code_root: "~/code".to_string(),
                org_root: "~/org".to_string(),
                no_ai: true,
                no_open: true,
                sync_only: false,
            };
            docs::ensure_docs_hub_daemon(&docs_opts)?;
            Ok(())
        }
        HubAction::Stop => {
            stop_daemon(opts)?;
            docs::stop_docs_hub_daemon()?;
            Ok(())
        }
    }
}

fn ensure_daemon(opts: HubOpts) -> Result<()> {
    let host = opts.host;
    let port = opts.port;

    if hub_healthy(host, port) {
        if !opts.no_ui {
            println!(
                "Lin watcher daemon already running at {}",
                format_addr(host, port)
            );
        }
        return Ok(());
    }

    supervisor::ensure_running(true, !opts.no_ui)?;

    let action = crate::cli::DaemonAction::Start {
        name: "lin".to_string(),
    };
    if !supervisor::try_handle_daemon_action(&action, None)? {
        daemon::start_daemon_with_path("lin", None)?;
    }

    if !opts.no_ui {
        println!("Lin watcher daemon ensured at {}", format_addr(host, port));
    }
    Ok(())
}

fn stop_daemon(opts: HubOpts) -> Result<()> {
    let action = crate::cli::DaemonAction::Stop {
        name: "lin".to_string(),
    };
    if supervisor::is_running() {
        if !supervisor::try_handle_daemon_action(&action, None)? {
            daemon::stop_daemon_with_path("lin", None)?;
        }
    } else {
        daemon::stop_daemon_with_path("lin", None)?;
    }
    if !opts.no_ui {
        println!("Lin hub stopped (if it was running).");
    }
    Ok(())
}

/// Check if the hub is healthy and responding.
pub fn hub_healthy(host: IpAddr, port: u16) -> bool {
    let url = format_health_url(host, port);
    let client = Client::builder()
        .timeout(Duration::from_millis(750))
        .build();

    let Ok(client) = client else {
        return false;
    };

    client
        .get(url)
        .send()
        .and_then(|resp| resp.error_for_status())
        .map(|_| true)
        .unwrap_or(false)
}

fn format_addr(host: IpAddr, port: u16) -> String {
    match host {
        IpAddr::V4(_) => format!("http://{host}:{port}"),
        IpAddr::V6(_) => format!("http://[{host}]:{port}"),
    }
}

fn format_health_url(host: IpAddr, port: u16) -> String {
    match host {
        IpAddr::V4(_) => format!("http://{host}:{port}/health"),
        IpAddr::V6(_) => format!("http://[{host}]:{port}/health"),
    }
}
