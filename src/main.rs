use anyhow::{Result, bail};
use clap::{Parser, error::ErrorKind};
use flowd::{
    cli::{Cli, Commands, TaskRunOpts, TasksOpts},
    doctor, history, hub, init, init_tracing, palette, processes, projects, tasks,
};
use std::net::IpAddr;

fn main() -> Result<()> {
    init_tracing();

    let raw_args: Vec<String> = std::env::args().collect();
    let cli = match Cli::try_parse_from(&raw_args) {
        Ok(cli) => cli,
        Err(err) => {
            if matches!(
                err.kind(),
                ErrorKind::UnknownArgument | ErrorKind::InvalidSubcommand
            ) {
                // Fallback: treat first positional as task name and rest as args.
                let mut iter = raw_args.into_iter();
                let _bin = iter.next();
                if let Some(task_name) = iter.next() {
                    let args: Vec<String> = iter.collect();
                    return tasks::run(TaskRunOpts {
                        config: TasksOpts::default().config,
                        delegate_to_hub: false,
                        hub_host: IpAddr::from([127, 0, 0, 1]),
                        hub_port: 9050,
                        name: task_name,
                        args,
                    });
                }
            }
            err.exit()
        }
    };
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
        Some(Commands::Tasks(opts)) => {
            tasks::list(opts)?;
        }
        Some(Commands::Run(opts)) => {
            tasks::run(opts)?;
        }
        Some(Commands::Search) => {
            palette::run_global()?;
        }
        Some(Commands::LastCmd) => {
            history::print_last_record()?;
        }
        Some(Commands::LastCmdFull) => {
            history::print_last_record_full()?;
        }
        Some(Commands::Ps(opts)) => {
            processes::show_project_processes(opts)?;
        }
        Some(Commands::Kill(opts)) => {
            processes::kill_processes(opts)?;
        }
        Some(Commands::Logs(opts)) => {
            processes::show_task_logs(opts)?;
        }
        Some(Commands::Projects) => {
            projects::show_projects()?;
        }
        Some(Commands::TaskShortcut(args)) => {
            let Some(task_name) = args.first() else {
                bail!("no task name provided");
            };
            tasks::run(TaskRunOpts {
                config: TasksOpts::default().config,
                delegate_to_hub: false,
                hub_host: IpAddr::from([127, 0, 0, 1]),
                hub_port: 9050,
                name: task_name.clone(),
                args: args[1..].to_vec(),
            })?;
        }
        None => {
            palette::run(TasksOpts::default())?;
        }
    }

    Ok(())
}
