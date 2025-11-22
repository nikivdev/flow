use anyhow::{Result, bail};
use clap::Parser;
use flowd::{
    cli::{Cli, Commands, TaskRunOpts, TasksOpts},
    doctor, hub, indexer, init_tracing, logs, palette, screen, secrets, server, servers_tui, setup,
    tasks, trace,
};

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
        Some(Commands::Hub(cmd)) => {
            hub::run(cmd)?;
        }
        Some(Commands::Setup(opts)) => {
            setup::run(opts)?;
        }
        Some(Commands::Doctor(opts)) => {
            doctor::run(opts)?;
        }
        Some(Commands::Tasks(opts)) => {
            tasks::list(opts)?;
        }
        Some(Commands::Activate(opts)) => {
            tasks::activate(opts)?;
        }
        Some(Commands::Run(opts)) => {
            tasks::run(opts)?;
        }
        Some(Commands::Secrets(cmd)) => {
            secrets::run(cmd)?;
        }
        Some(Commands::Index(opts)) => {
            indexer::run(opts)?;
        }
        Some(Commands::Logs(opts)) => {
            logs::run(opts)?;
        }
        Some(Commands::Trace(opts)) => {
            trace::run(opts)?;
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
            palette::run(TasksOpts::default())?;
        }
    }

    Ok(())
}
