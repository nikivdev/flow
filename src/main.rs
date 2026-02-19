use std::net::IpAddr;
use std::path::Path;
use std::time::Instant;

use anyhow::{Result, bail};
use clap::{Parser, error::ErrorKind};
use flowd::{
    agents, ai, ai_test, analytics, archive, auth, branches, changes,
    cli::{
        Cli, Commands, InstallAction, ProxyAction, ProxyCommand, RerunOpts, ReviewAction,
        ShellAction, ShellCommand, TaskRunOpts, TasksOpts, TraceAction,
    },
    code, commit, commits, daemon, deploy, deps, docs, doctor, domains, env, explain_commits, ext,
    fish_install, fish_trace, fix, fixup, git_guard, gitignore_policy, hash, health, help_search,
    history, hive, home, hub, info, init, init_tracing, install, invariants, jj, latest,
    log_server, macos, notify, otp, palette, parallel, processes, projects, proxy, publish, push,
    recipe, registry, release, repos, reviews_todo, seq_rpc, services, setup, skills, ssh_keys,
    storage, supervisor, sync, task_match, tasks, todo, tools, traces, undo, upgrade, upstream,
    usage, web,
};

fn main() -> Result<()> {
    init_tracing();
    flowd::config::load_global_secrets();

    let raw_args: Vec<String> = std::env::args().collect();
    let analytics_capture = usage::command_capture(&raw_args);
    let is_analytics_command = usage::is_analytics_command(&raw_args);
    let started_at = Instant::now();

    let result = (|| -> Result<()> {
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

        // Keep default skills in sync for Flow projects (minimal cost).
        skills::auto_sync_skills();

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
            Some(Commands::Home(cmd)) => {
                home::run(cmd)?;
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
            Some(Commands::Invariants(opts)) => {
                let root = std::env::current_dir()?;
                invariants::check(&root, opts.staged)?;
            }
            Some(Commands::Tasks(cmd)) => {
                tasks::run_tasks_command(cmd)?;
            }
            Some(Commands::Fast(opts)) => {
                tasks::run_fast(opts)?;
            }
            Some(Commands::AiTestNew(opts)) => {
                ai_test::run(opts)?;
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
                // Prefer fish shell traces if available, fall back to flow history
                if fish_trace::load_last_record()?.is_some() {
                    fish_trace::print_last_fish_cmd()?;
                } else {
                    history::print_last_record()?;
                }
            }
            Some(Commands::LastCmdFull) => {
                // Prefer fish shell traces if available, fall back to flow history
                if fish_trace::load_last_record()?.is_some() {
                    fish_trace::print_last_fish_cmd_full()?;
                } else {
                    history::print_last_record_full()?;
                }
            }
            Some(Commands::FishLast) => {
                fish_trace::print_last_fish_cmd()?;
            }
            Some(Commands::FishLastFull) => {
                fish_trace::print_last_fish_cmd_full()?;
            }
            Some(Commands::FishInstall(opts)) => {
                fish_install::run(opts)?;
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
            Some(Commands::Ask(opts)) => {
                flowd::ask::run(flowd::ask::AskOpts {
                    args: opts.query,
                    model: opts.model,
                    url: opts.url,
                })?;
            }
            Some(Commands::Branches(cmd)) => {
                branches::run(cmd)?;
            }
            Some(Commands::Commit(opts)) => {
                // Default: fast commit lane with deferred Codex deep review.
                let mut force = opts.force || opts.approved;
                let mut message_arg = opts.message_arg.as_deref();
                let mut open_review = opts.review;
                if !force {
                    if let Some(arg) = message_arg {
                        if arg == "force"
                            && opts.message.is_none()
                            && opts.fast.is_none()
                            && !opts.queue
                            && !opts.no_queue
                        {
                            force = true;
                            message_arg = None;
                        } else if arg == "review"
                            && opts.message.is_none()
                            && opts.fast.is_none()
                            && !opts.queue
                            && !opts.no_queue
                        {
                            open_review = true;
                            message_arg = None;
                        }
                    }
                }
                let queue = commit::resolve_commit_queue_mode(opts.queue, opts.no_queue || force)
                    .with_open_review(open_review);
                let push = !opts.no_push;
                let explicit_blocking = opts.slow
                    || opts.dry
                    || opts.context
                    || opts.sync
                    || opts.codex
                    || opts.review_model.is_some()
                    || opts.skip_quality
                    || opts.skip_docs
                    || opts.skip_tests
                    || opts.message.is_some()
                    || message_arg.is_some();
                let implicit_quick = !opts.quick
                    && !explicit_blocking
                    && opts.fast.is_none()
                    && commit::commit_quick_default_enabled();
                if opts.quick || implicit_quick {
                    if implicit_quick {
                        println!(
                            "ℹ️  using fast commit + deferred Codex deep review by default. Pass --slow for blocking pre-commit review."
                        );
                    }
                    commit::run_quick_then_async_review(
                        push,
                        queue,
                        opts.hashed,
                        &opts.paths,
                        opts.fast.as_deref(),
                    )?;
                    return Ok(());
                }
                if let Some(message) = opts.fast.as_deref() {
                    commit::run_fast(message, push, queue, opts.hashed, &opts.paths)?;
                    return Ok(());
                }
                let review_selection =
                    commit::resolve_review_selection_v2(opts.codex, opts.review_model.clone());
                let author_message = opts.message.as_deref().or(message_arg);
                if opts.dry {
                    commit::dry_run_context()?;
                } else if opts.sync {
                    commit::run_with_check_sync(
                        push,
                        opts.context,
                        review_selection,
                        author_message,
                        opts.tokens,
                        true,
                        queue,
                        opts.hashed,
                        &opts.paths,
                        commit::CommitGateOverrides {
                            skip_quality: opts.skip_quality,
                            skip_docs: opts.skip_docs,
                            skip_tests: opts.skip_tests,
                        },
                    )?;
                } else {
                    commit::run_with_check_with_gitedit(
                        push,
                        opts.context,
                        review_selection,
                        author_message,
                        opts.tokens,
                        queue,
                        opts.hashed,
                        &opts.paths,
                        commit::CommitGateOverrides {
                            skip_quality: opts.skip_quality,
                            skip_docs: opts.skip_docs,
                            skip_tests: opts.skip_tests,
                        },
                    )?;
                }
            }
            Some(Commands::CommitQueue(cmd)) => {
                commit::run_commit_queue(cmd)?;
            }
            Some(Commands::ReviewsTodo(cmd)) => {
                reviews_todo::run(cmd)?;
            }
            Some(Commands::Pr(opts)) => {
                commit::run_pr(opts)?;
            }
            Some(Commands::Gitignore(cmd)) => {
                gitignore_policy::run(cmd)?;
            }
            Some(Commands::Recipe(cmd)) => {
                recipe::run(cmd)?;
            }
            Some(Commands::Review(cmd)) => match cmd.action {
                Some(ReviewAction::Latest) | None => {
                    commit::open_latest_queue_review()?;
                }
                Some(ReviewAction::Copy { hash }) => {
                    commit::copy_review_prompt(hash.as_deref())?;
                }
            },
            Some(Commands::GitRepair(opts)) => {
                git_guard::run_git_repair(opts)?;
            }
            Some(Commands::Jj(cmd)) => {
                jj::run(cmd)?;
            }
            Some(Commands::CommitSimple(opts)) => {
                // Simple commit without review - always sync (fast, no hub)
                let mut force = opts.force || opts.approved;
                let mut open_review = opts.review;
                if !force {
                    if let Some(arg) = opts.message_arg.as_deref() {
                        if arg == "force" && opts.message.is_none() && opts.fast.is_none() {
                            force = true;
                        } else if arg == "review"
                            && opts.message.is_none()
                            && opts.fast.is_none()
                            && !opts.queue
                            && !opts.no_queue
                        {
                            open_review = true;
                        }
                    }
                }
                let queue = commit::resolve_commit_queue_mode(opts.queue, opts.no_queue || force)
                    .with_open_review(open_review);
                let push = !opts.no_push;
                commit::run_sync(push, queue, opts.hashed, &opts.paths)?;
            }
            Some(Commands::CommitWithCheck(opts)) => {
                // Review but no gitedit sync
                let mut force = opts.force || opts.approved;
                let mut open_review = opts.review;
                if !force {
                    if let Some(arg) = opts.message_arg.as_deref() {
                        if arg == "force" && opts.message.is_none() && opts.fast.is_none() {
                            force = true;
                        } else if arg == "review"
                            && opts.message.is_none()
                            && opts.fast.is_none()
                            && !opts.queue
                            && !opts.no_queue
                        {
                            open_review = true;
                        }
                    }
                }
                let queue = commit::resolve_commit_queue_mode(opts.queue, opts.no_queue || force)
                    .with_open_review(open_review);
                let push = !opts.no_push;
                if opts.quick {
                    commit::run_quick_then_async_review(
                        push,
                        queue,
                        opts.hashed,
                        &opts.paths,
                        opts.fast.as_deref(),
                    )?;
                    return Ok(());
                }
                let review_selection =
                    commit::resolve_review_selection_v2(opts.codex, opts.review_model.clone());
                if opts.dry {
                    commit::dry_run_context()?;
                } else if opts.sync {
                    commit::run_with_check_sync(
                        push,
                        opts.context,
                        review_selection,
                        opts.message.as_deref(),
                        opts.tokens,
                        false,
                        queue,
                        opts.hashed,
                        &opts.paths,
                        commit::CommitGateOverrides {
                            skip_quality: opts.skip_quality,
                            skip_docs: opts.skip_docs,
                            skip_tests: opts.skip_tests,
                        },
                    )?;
                } else {
                    commit::run_with_check(
                        push,
                        opts.context,
                        review_selection,
                        opts.message.as_deref(),
                        opts.tokens,
                        queue,
                        opts.hashed,
                        &opts.paths,
                        commit::CommitGateOverrides {
                            skip_quality: opts.skip_quality,
                            skip_docs: opts.skip_docs,
                            skip_tests: opts.skip_tests,
                        },
                    )?;
                }
            }
            Some(Commands::Fix(opts)) => {
                fix::run(opts)?;
            }
            Some(Commands::Undo(cmd)) => {
                undo::run(cmd)?;
            }
            Some(Commands::Fixup(opts)) => {
                fixup::run(opts)?;
            }
            Some(Commands::Changes(cmd)) => {
                changes::run(cmd)?;
            }
            Some(Commands::Diff(cmd)) => {
                changes::run_diff(cmd)?;
            }
            Some(Commands::Hash(opts)) => {
                hash::run(opts)?;
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
            Some(Commands::Otp(cmd)) => {
                otp::run(cmd)?;
            }
            Some(Commands::Auth(opts)) => {
                auth::run(opts)?;
            }
            Some(Commands::Services(cmd)) => {
                services::run(cmd)?;
            }
            Some(Commands::Macos(cmd)) => {
                macos::run(cmd)?;
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
            Some(Commands::SeqRpc(cmd)) => {
                seq_rpc::run(cmd)?;
            }
            Some(Commands::ExplainCommits(cmd)) => {
                explain_commits::run_cli(cmd)?;
            }
            Some(Commands::Setup(opts)) => {
                setup::run(opts)?;
            }
            Some(Commands::Agents(cmd)) => {
                agents::run(cmd)?;
            }
            Some(Commands::Hive(cmd)) => {
                hive::run_command(cmd)?;
            }
            Some(Commands::Sync(cmd)) => {
                sync::run(cmd)?;
            }
            Some(Commands::Checkout(cmd)) => {
                sync::run_checkout(cmd)?;
            }
            Some(Commands::Switch(cmd)) => {
                sync::run_switch(cmd)?;
            }
            Some(Commands::Push(cmd)) => {
                push::run(cmd)?;
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
            Some(Commands::Latest) => {
                latest::run()?;
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
            Some(Commands::Analytics(cmd)) => {
                analytics::run(cmd)?;
            }
            Some(Commands::Proxy(cmd)) => {
                proxy_command(cmd)?;
            }
            Some(Commands::Domains(cmd)) => {
                domains::run(cmd)?;
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
    })();

    usage::record_command_result(&analytics_capture, started_at.elapsed(), &result);
    usage::maybe_prompt_for_opt_in(is_analytics_command, result.is_ok());
    result
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
        println!(
            "Refresh your shell session (fish): source {}",
            config_path.display()
        );
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
            println!(
                "Manage your fish config manually: {}",
                config_fish.display()
            );
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

/// Handle proxy commands
fn proxy_command(cmd: ProxyCommand) -> Result<()> {
    // Helper to load config from current directory
    let load_project_config = || -> Result<flowd::config::Config> {
        let cwd = std::env::current_dir()?;
        let flow_toml = cwd.join("flow.toml");
        if flow_toml.exists() {
            flowd::config::load(&flow_toml)
        } else {
            // Try global config
            let global = dirs::config_dir()
                .map(|d| d.join("flow").join("flow.toml"))
                .filter(|p| p.exists());
            if let Some(path) = global {
                flowd::config::load(&path)
            } else {
                bail!("No flow.toml found in current directory or global config");
            }
        }
    };

    match cmd.action {
        ProxyAction::Start(opts) => {
            // Load config
            let config = load_project_config()?;
            let proxy_config = config.proxy.unwrap_or_default();
            let targets = config.proxies;

            if targets.is_empty() {
                bail!("No proxy targets configured. Add [[proxies]] to flow.toml");
            }

            // Override listen if provided
            let proxy_config = if let Some(listen) = opts.listen {
                proxy::ProxyConfig {
                    listen,
                    ..proxy_config
                }
            } else {
                proxy_config
            };

            // Start server
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(proxy::start(proxy_config, targets))?;
        }
        ProxyAction::Trace(opts) => {
            proxy::trace_last(opts.count)?;
        }
        ProxyAction::Last(_opts) => {
            proxy::trace_last(1)?;
        }
        ProxyAction::Add(opts) => {
            println!("To add a proxy, edit flow.toml:");
            println!();
            println!("[[proxies]]");
            println!(
                "name = \"{}\"",
                opts.name.unwrap_or_else(|| "myservice".to_string())
            );
            println!("target = \"{}\"", opts.target);
            if let Some(host) = opts.host {
                println!("host = \"{}\"", host);
            }
            if let Some(path) = opts.path {
                println!("path = \"{}\"", path);
            }
        }
        ProxyAction::List => {
            let config = load_project_config()?;
            if config.proxies.is_empty() {
                println!("No proxy targets configured.");
                println!("Add [[proxies]] sections to flow.toml");
            } else {
                println!(
                    "{:<15} {:<25} {:<15} {:<15}",
                    "NAME", "TARGET", "HOST", "PATH"
                );
                println!("{}", "-".repeat(70));
                for p in &config.proxies {
                    println!(
                        "{:<15} {:<25} {:<15} {:<15}",
                        p.name,
                        p.target,
                        p.host.as_deref().unwrap_or("-"),
                        p.path.as_deref().unwrap_or("-")
                    );
                }
            }
        }
        ProxyAction::Stop => {
            println!("Proxy stop not implemented yet. Use Ctrl+C or kill the process.");
        }
    }
    Ok(())
}
