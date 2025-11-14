mod cli;
mod config;
mod screen;
mod server;
mod servers;
mod servers_tui;
mod setup;
mod tasks;

use anyhow::{Result, bail};
use clap::Parser;
use cli::{Cli, Commands, TaskRunOpts, TasksOpts};

fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Daemon(opts)) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(server::run(opts))?;
        }
        Some(Commands::Screen(opts)) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(screen::preview(opts))?;
        }
        Some(Commands::Servers(opts)) => {
            servers_tui::run(opts)?;
        }
        Some(Commands::Setup(opts)) => {
            setup::run(opts)?;
        }
        Some(Commands::Tasks(opts)) => {
            tasks::list(opts)?;
        }
        Some(Commands::Run(opts)) => {
            tasks::run(opts)?;
        }
        Some(Commands::TaskShortcut(args)) => {
            let Some(task_name) = args.first() else {
                bail!("no task name provided");
            };
            if args.len() > 1 {
                bail!(
                    "task '{}' does not accept additional arguments: {}",
                    task_name,
                    args[1..].join(" ")
                );
            }
            tasks::run(TaskRunOpts {
                config: TasksOpts::default().config,
                name: task_name.clone(),
            })?;
        }
        None => {
            tasks::list(TasksOpts::default())?;
        }
    }

    Ok(())
}

fn init_tracing() {
    let default_filter = "flowd=info,axum=warn,tower=warn";
    let filter_layer = std::env::var("RUST_LOG").unwrap_or_else(|_| default_filter.to_string());

    tracing_subscriber::fmt()
        .with_env_filter(filter_layer)
        .with_target(false)
        .compact()
        .init();
}
