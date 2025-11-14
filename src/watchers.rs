use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_mini::{DebouncedEvent, new_debouncer};
use shellexpand::tilde;

use crate::config::WatcherConfig;

pub struct WatchManager {
    handles: Vec<WatcherHandle>,
}

impl WatchManager {
    pub fn start(configs: &[WatcherConfig]) -> Result<Option<Self>> {
        if configs.is_empty() {
            return Ok(None);
        }

        let mut handles = Vec::new();
        for cfg in configs.iter().cloned() {
            match WatcherHandle::spawn(cfg) {
                Ok(handle) => handles.push(handle),
                Err(err) => {
                    tracing::error!(?err, "failed to start watcher");
                }
            }
        }

        if handles.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Self { handles }))
        }
    }
}

impl Drop for WatchManager {
    fn drop(&mut self) {
        self.handles.clear();
    }
}

pub struct WatcherHandle {
    shutdown: Option<Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl WatcherHandle {
    fn spawn(cfg: WatcherConfig) -> Result<Self> {
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            if let Err(err) = run_watcher(cfg, shutdown_rx) {
                tracing::error!(?err, "watcher exited with error");
            }
        });

        Ok(Self {
            shutdown: Some(shutdown_tx),
            join: Some(handle),
        })
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

fn run_watcher(cfg: WatcherConfig, shutdown: Receiver<()>) -> Result<()> {
    let watch_path = expand_path(&cfg.path);
    if !watch_path.exists() {
        anyhow::bail!(
            "watch path {} does not exist (watcher {})",
            watch_path.display(),
            cfg.name
        );
    }

    let workdir = if watch_path.is_dir() {
        watch_path.clone()
    } else {
        watch_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };

    if cfg.run_on_start {
        run_command(&cfg, &workdir);
    }

    let debounce = Duration::from_millis(cfg.debounce_ms.max(50));
    let (event_tx, event_rx) = mpsc::channel();
    let mut debouncer =
        new_debouncer(debounce, event_tx).context("failed to initialize file watcher")?;

    debouncer
        .watcher()
        .watch(&watch_path, RecursiveMode::Recursive)
        .with_context(|| format!("failed to watch path {}", watch_path.display()))?;

    tracing::info!(
        name = cfg.name,
        path = %watch_path.display(),
        "watcher started"
    );

    loop {
        if shutdown.try_recv().is_ok() {
            break;
        }

        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(events)) => {
                if matches_filter(&events, cfg.filter.as_deref()) {
                    run_command(&cfg, &workdir);
                }
            }
            Ok(Err(err)) => {
                tracing::warn!(?err, watcher = cfg.name, "watcher error");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    tracing::info!(name = cfg.name, "watcher stopped");
    Ok(())
}

fn matches_filter(events: &[DebouncedEvent], filter: Option<&str>) -> bool {
    match filter {
        None => true,
        Some(target) => events.iter().any(|event| {
            event
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name == target || name.contains(target))
                .unwrap_or(false)
        }),
    }
}

fn run_command(cfg: &WatcherConfig, workdir: &Path) {
    tracing::info!(
        name = cfg.name,
        command = cfg.command,
        "running watcher command"
    );
    let start = Instant::now();
    match Command::new("/bin/sh")
        .arg("-c")
        .arg(cfg.command.trim())
        .current_dir(workdir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            let _ = child.wait();
            tracing::info!(name = cfg.name, ?workdir, elapsed = ?start.elapsed(), "watcher command finished");
        }
        Err(err) => {
            tracing::error!(?err, name = cfg.name, "failed to execute watcher command");
        }
    }
}

fn expand_path(raw: &str) -> PathBuf {
    let tilde_expanded = tilde(raw).into_owned();
    let env_expanded = match shellexpand::env(&tilde_expanded) {
        Ok(val) => val.into_owned(),
        Err(_) => tilde_expanded,
    };
    PathBuf::from(env_expanded)
}
