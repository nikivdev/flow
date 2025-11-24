use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;

use flowd::{
    config, init_tracing,
    lin_runtime::{self, LinRuntime},
    watchers::WatchManager,
};

/// Standalone watcher daemon that mirrors the watch config from flow.
#[derive(Parser, Debug)]
#[command(
    name = "lin",
    version,
    about = "Lean watcher daemon that reloads on config changes",
    arg_required_else_help = false
)]
struct LinCli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the flow config TOML (defaults to ~/.config/flow/flow.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    /// Run the watcher daemon (default if no subcommand is provided).
    Daemon,
    /// Register this lin binary so `flow` can launch it automatically.
    Register,
}

fn main() -> Result<()> {
    init_tracing();

    let cli = LinCli::parse();
    let command = cli.command.unwrap_or(Command::Daemon);

    match command {
        Command::Daemon => {
            let config_path = resolve_config_path(cli.config);
            run(config_path)
        }
        Command::Register => register_runtime(),
    }
}

fn run(config_path: PathBuf) -> Result<()> {
    tracing::info!(path = %config_path.display(), "starting lin watcher daemon");
    let (reload_tx, reload_rx) = mpsc::channel();
    let (shutdown_tx, shutdown_rx) = mpsc::channel();

    if let Err(err) = spawn_config_watcher(&config_path, reload_tx.clone()) {
        tracing::warn!(
            ?err,
            "failed to watch config for reloads; continuing without hot reload"
        );
    }
    spawn_shutdown_listener(shutdown_tx);

    let mut manager = start_watchers(&config_path)?;
    println!(
        "lin watcher daemon ready (config: {})",
        config_path.display()
    );
    println!("Press Ctrl+C to stop.");

    loop {
        if shutdown_rx.try_recv().is_ok() {
            break;
        }

        match reload_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(()) => {
                tracing::info!("config change detected; reloading watchers");
                manager = start_watchers(&config_path)?;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    drop(manager);
    tracing::info!("lin watcher daemon shutting down");
    Ok(())
}

fn register_runtime() -> Result<()> {
    let binary = std::env::current_exe().context("failed to resolve current executable")?;
    let runtime = LinRuntime {
        version: lin_runtime::detect_binary_version(&binary),
        binary: binary.clone(),
    };

    lin_runtime::persist_runtime(&runtime)?;
    println!(
        "Registered lin for flow at {}\nMetadata written to {}",
        binary.display(),
        lin_runtime::runtime_path().display()
    );

    Ok(())
}

fn resolve_config_path(raw: Option<PathBuf>) -> PathBuf {
    raw.map(|path| {
        if path.is_absolute() {
            path
        } else {
            config::expand_path(&path.to_string_lossy())
        }
    })
    .unwrap_or_else(config::default_config_path)
}

fn start_watchers(config_path: &Path) -> Result<Option<WatchManager>> {
    let cfg = config::load_or_default(config_path);
    let count = cfg.watchers.len();
    let manager = WatchManager::start(&cfg.watchers)
        .with_context(|| format!("failed to start watchers from {}", config_path.display()))?;

    if count == 0 {
        tracing::info!(path = %config_path.display(), "no watchers defined; idling");
    } else {
        tracing::info!(path = %config_path.display(), count, "watchers started");
    }

    Ok(manager)
}

fn spawn_config_watcher(path: &Path, tx: mpsc::Sender<()>) -> notify::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let watch_root = target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| target.clone());

    thread::spawn(move || {
        let (event_tx, event_rx) = mpsc::channel();
        let mut debouncer = match new_debouncer(Duration::from_millis(250), event_tx) {
            Ok(debouncer) => debouncer,
            Err(err) => {
                tracing::error!(?err, "failed to initialize config watcher");
                return;
            }
        };

        if let Err(err) = debouncer
            .watcher()
            .watch(&watch_root, RecursiveMode::NonRecursive)
        {
            tracing::error!(
                ?err,
                path = %watch_root.display(),
                "failed to watch config directory"
            );
            return;
        }

        while let Ok(result) = event_rx.recv() {
            match result {
                Ok(events) => {
                    let should_reload = events.iter().any(|event| same_file(&target, &event.path));
                    if should_reload && tx.send(()).is_err() {
                        break;
                    }
                }
                Err(err) => tracing::warn!(?err, "config watcher error"),
            }
        }
    });

    Ok(())
}

fn spawn_shutdown_listener(tx: mpsc::Sender<()>) {
    thread::spawn(move || {
        if let Ok(rt) = tokio::runtime::Runtime::new() {
            let _ = rt.block_on(tokio::signal::ctrl_c());
        }
        let _ = tx.send(());
    });
}

fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }

    if let Ok(canon) = b.canonicalize() {
        if canon == a {
            return true;
        }
    }

    a.file_name()
        .is_some_and(|name| Some(name) == b.file_name())
}
