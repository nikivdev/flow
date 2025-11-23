use anyhow::{Result, bail};
use clap::Parser;
use flowd::{
    cli::{Cli, Commands, TaskRunOpts, TasksOpts},
    doctor, hub, init, init_tracing, palette, tasks,
};

fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Hub(cmd)) => {
            hub::run(cmd)?;
        }
        Some(Commands::Init(opts)) => {
            init::run(opts)?;
        }
        Some(Commands::Doctor(opts)) => {
            doctor::run(opts)?;
        }
        Some(Commands::Run(opts)) => {
            tasks::run(opts)?;
        }
        Some(Commands::Search) => {
            palette::run_global()?;
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
