use std::net::IpAddr;
use std::path::Path;

use anyhow::{Result, bail};
use clap::{Parser, error::ErrorKind};
use flowd::{
    agents, ai, archive, auth,
    cli::{Cli, Commands, InstallAction, RerunOpts, ShellAction, ShellCommand, TaskRunOpts, TasksOpts, TraceAction},
    code, commit, commits, daemon, deploy, deps, docs, doctor, env, ext, fixup, health, help_search,
    history, home, hub, info, init, init_tracing, install, log_server, notify, palette, parallel,
    processes, projects,
    publish, registry, release, repos, services, setup, skills, ssh_keys, storage, supervisor,
    sync, task_match, tasks, todo, tools, traces, upgrade, upstream, web,
};

fn main() -> Result<()> {
    init_tracing();
    flowd::config::load_global_secrets();

    let raw_args: Vec<String> = std::env::args().collect();

    // Handle `f ?` for fuzzy help search before clap parsing
    if raw_args.get(1).map(|s| s.as_str()) == Some("?") {
        return help_search::run();
    }

    // Handle --help-full early for instant output
    if raw_args.iter().any(|s| s == "--help-full") {
        return help_search::print_full_json();
    }

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
                    return tasks::run_with_discovery(&task_name, args);
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
        Some(Commands::ShellInit(opts)) => {
            shell_init(&opts.shell);
        }
        Some(Commands::Shell(cmd)) => {
            shell_command(cmd);
        }
        Some(Commands::New(opts)) => {
            code::new_from_template(opts)?;
        }
        Some(Commands::Home(opts)) => {
            home::run(opts)?;
        }
        Some(Commands::Archive(opts)) => {
            archive::run(opts)?;
        }
        Some(Commands::Doctor(opts)) => {
            doctor::run(opts)?;
        }
        Some(Commands::Health(opts)) => {
            health::run(opts)?;
        }
        Some(Commands::Tasks(cmd)) => {
            tasks::run_tasks_command(cmd)?;
        }
        Some(Commands::Global(cmd)) => {
            tasks::run_global(cmd)?;
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
        Some(Commands::Rerun(opts)) => {
            rerun(opts)?;
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
        Some(Commands::Trace(cmd)) => {
            if let Some(action) = cmd.action {
                match action {
                    TraceAction::Session(opts) => {
                        traces::run_session(opts)?;
                    }
                }
            } else {
                traces::run(cmd.events)?;
            }
        }
        Some(Commands::Projects) => {
            projects::show_projects()?;
        }
        Some(Commands::Sessions(opts)) => {
            ai::run_sessions(&opts)?;
        }
        Some(Commands::Active(opts)) => {
            projects::handle_active(opts)?;
        }
        Some(Commands::Server(opts)) => {
            log_server::run(opts)?;
        }
        Some(Commands::Web(opts)) => {
            web::run(opts)?;
        }
        Some(Commands::Match(opts)) => {
            task_match::run(task_match::MatchOpts {
                args: opts.query,
                model: opts.model,
                port: Some(opts.port),
                execute: !opts.dry_run,
            })?;
        }
        Some(Commands::Commit(opts)) => {
            // Default: Claude review, no context, gitedit sync
            let review_selection =
                commit::resolve_review_selection_v2(opts.codex, opts.review_model.clone());
            if opts.dry {
                commit::dry_run_context()?;
            } else if opts.sync {
                commit::run_with_check_sync(
                    !opts.no_push,
                    opts.context,
                    review_selection,
                    opts.message.as_deref(),
                    opts.tokens,
                    true,
                )?;
            } else {
                commit::run_with_check_with_gitedit(
                    !opts.no_push,
                    opts.context,
                    review_selection,
                    opts.message.as_deref(),
                    opts.tokens,
                )?;
            }
        }
        Some(Commands::CommitSimple(opts)) => {
            // Simple commit without review
            if opts.sync {
                commit::run_sync(!opts.no_push)?;
            } else {
                commit::run(!opts.no_push)?;
            }
        }
        Some(Commands::CommitWithCheck(opts)) => {
            // Review but no gitedit sync
            let review_selection =
                commit::resolve_review_selection_v2(opts.codex, opts.review_model.clone());
            if opts.dry {
                commit::dry_run_context()?;
            } else if opts.sync {
                commit::run_with_check_sync(
                    !opts.no_push,
                    opts.context,
                    review_selection,
                    opts.message.as_deref(),
                    opts.tokens,
                    false,
                )?;
            } else {
                commit::run_with_check(
                    !opts.no_push,
                    opts.context,
                    review_selection,
                    opts.message.as_deref(),
                    opts.tokens,
                )?;
            }
        }
        Some(Commands::Fixup(opts)) => {
            fixup::run(opts)?;
        }
        Some(Commands::Daemon(cmd)) => {
            daemon::run(cmd)?;
        }
        Some(Commands::Supervisor(cmd)) => {
            supervisor::run(cmd)?;
        }
        Some(Commands::Ai(cmd)) => {
            ai::run(cmd.action)?;
        }
        Some(Commands::Codex { action }) => {
            ai::run_provider(ai::Provider::Codex, action)?;
        }
        Some(Commands::Claude { action }) => {
            ai::run_provider(ai::Provider::Claude, action)?;
        }
        Some(Commands::Env(cmd)) => {
            env::run(cmd.action)?;
        }
        Some(Commands::Auth(opts)) => {
            auth::run(opts)?;
        }
        Some(Commands::Services(cmd)) => {
            services::run(cmd)?;
        }
        Some(Commands::Ssh(cmd)) => {
            ssh_keys::run(cmd.action)?;
        }
        Some(Commands::Todo(cmd)) => {
            todo::run(cmd)?;
        }
        Some(Commands::Ext(cmd)) => {
            ext::run(cmd)?;
        }
        Some(Commands::Skills(cmd)) => {
            skills::run(cmd)?;
        }
        Some(Commands::Deps(cmd)) => {
            deps::run(cmd)?;
        }
        Some(Commands::Db(cmd)) => {
            storage::run(cmd)?;
        }
        Some(Commands::Tools(cmd)) => {
            tools::run(cmd)?;
        }
        Some(Commands::Notify(cmd)) => {
            notify::run(cmd)?;
        }
        Some(Commands::Commits(cmd)) => {
            commits::run(cmd)?;
        }
        Some(Commands::Setup(opts)) => {
            setup::run(opts)?;
        }
        Some(Commands::Agents(cmd)) => {
            agents::run(cmd)?;
        }
        Some(Commands::Sync(cmd)) => {
            sync::run(cmd)?;
        }
        Some(Commands::Info) => {
            info::run()?;
        }
        Some(Commands::Upstream(cmd)) => {
            upstream::run(cmd)?;
        }
        Some(Commands::Deploy(cmd)) => {
            deploy::run(cmd)?;
        }
        Some(Commands::Prod(cmd)) => {
            deploy::run_prod(cmd)?;
        }
        Some(Commands::Publish(cmd)) => {
            publish::run(cmd)?;
        }
        Some(Commands::Repos(cmd)) => {
            repos::run(cmd)?;
        }
        Some(Commands::Code(cmd)) => {
            code::run(cmd)?;
        }
        Some(Commands::Migrate(cmd)) => {
            code::run_migrate(cmd)?;
        }
        Some(Commands::Parallel(cmd)) => {
            parallel::run(cmd)?;
        }
        Some(Commands::Docs(cmd)) => {
            docs::run(cmd)?;
        }
        Some(Commands::Upgrade(opts)) => {
            upgrade::run(opts)?;
        }
        Some(Commands::Release(cmd)) => {
            release::run(cmd)?;
        }
        Some(Commands::Install(cmd)) => {
            if let Some(InstallAction::Index(opts)) = cmd.action.clone() {
                install::run_index(opts)?;
            } else {
                install::run(cmd.opts)?;
            }
        }
        Some(Commands::Registry(cmd)) => {
            registry::run(cmd)?;
        }
        Some(Commands::TaskShortcut(args)) => {
            let Some(task_name) = args.first() else {
                bail!("no task name provided");
            };
            if let Err(err) = tasks::run_with_discovery(task_name, args[1..].to_vec()) {
                if is_task_not_found(&err) {
                    return Err(err);
                }
                return Err(err);
            }
        }
        None => {
            palette::run(TasksOpts::default())?;
        }
    }

    Ok(())
}

fn rerun(opts: RerunOpts) -> Result<()> {
    let project_root = if opts.config.is_absolute() {
        opts.config.parent().unwrap_or(Path::new(".")).to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf())
    };

    let record = history::load_last_record_for_project(&project_root)?;
    let Some(rec) = record else {
        bail!("no previous task found for this project");
    };

    // Parse user_input to extract task name and args (respecting shell quoting)
    let parts = shell_words::split(&rec.user_input).unwrap_or_else(|_| vec![rec.task_name.clone()]);
    let task_name = parts.first().cloned().unwrap_or(rec.task_name.clone());
    let args: Vec<String> = parts.into_iter().skip(1).collect();

    println!("Re-running: {}", rec.user_input);

    tasks::run(TaskRunOpts {
        config: opts.config,
        delegate_to_hub: false,
        hub_host: IpAddr::from([127, 0, 0, 1]),
        hub_port: 9050,
        name: task_name,
        args,
    })
}

fn is_task_not_found(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("task '") && msg.contains("not found")
}

fn shell_command(cmd: ShellCommand) {
    match cmd.action.unwrap_or(ShellAction::Reset) {
        ShellAction::Reset => {
            shell_reset();
        }
        ShellAction::FixTerminal => {
            shell_fix_terminal();
        }
    }
}

fn shell_reset() {
    let home = dirs::home_dir().expect("no home directory");
    let config_path = home.join("config").join("fish").join("config.fish");
    if std::env::var("FISH_VERSION").is_ok() {
        println!("Run: source {}", config_path.display());
    } else {
        println!("Refresh your shell session (fish): source {}", config_path.display());
    }
}

fn shell_fix_terminal() {
    let status = std::process::Command::new("fish")
        .arg("-c")
        .arg("set -Ua fish_features no-query-term")
        .status();
    match status {
        Ok(status) if status.success() => {
            println!("Disabled fish terminal query (no-query-term). Restart fish to apply.");
        }
        _ => {
            println!("Run in fish: set -Ua fish_features no-query-term");
            println!("Then restart fish to apply.");
        }
    }
}

fn shell_init(shell: &str) {
    use std::fs;
    use std::io::Write;

    let home = dirs::home_dir().expect("no home directory");
    let config_dir = home.join("config");

    match shell {
        "fish" => {
            let config_fish = config_dir.join("fish").join("config.fish");

            println!("No fish integration changes applied.");
            println!("Manage your fish config manually: {}", config_fish.display());
        }
        "zsh" => {
            let zshrc = config_dir.join("zsh").join(".zshrc");

            if zshrc.exists() {
                let content = fs::read_to_string(&zshrc).unwrap_or_default();
                if content.contains("# flow:start") {
                    println!("Already set up in {}", zshrc.display());
                    return;
                }
            }

            let snippet = r#"
# flow:start
f() {
    local bin
    if [[ -x ~/.local/bin/f ]]; then
        bin=~/.local/bin/f
    else
        bin=$(command -v f)
    fi

    case "$1" in
        new)
            local output
            output=$("$bin" "$@" 2>&1)
            echo "$output"
            local created
            created=$(echo "$output" | grep -oE 'Created .+' | cut -d' ' -f2-)
            if [[ -n "$created" && -d "$created" ]]; then
                cd "$created"
            fi
            ;;
        *)
            "$bin" "$@"
            ;;
    esac
}
# flow:end
"#;

            let mut file = match fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&zshrc)
            {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("Failed to open {}: {}", zshrc.display(), e);
                    return;
                }
            };

            if let Err(e) = file.write_all(snippet.as_bytes()) {
                eprintln!("Failed to write to {}: {}", zshrc.display(), e);
                return;
            }

            println!("Added flow integration to {}", zshrc.display());
        }
        _ => {
            eprintln!("Unsupported shell: {}", shell);
            eprintln!("Supported: fish, zsh");
        }
    }
}
