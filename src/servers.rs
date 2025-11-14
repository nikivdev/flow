use std::{
    collections::VecDeque,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{RwLock, broadcast},
};

use crate::config::ServerConfig;

/// Origin of a log line (stdout or stderr).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// Single log entry from a managed server process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    /// Name of the server that produced this line.
    pub server: String,
    /// Milliseconds since UNIX epoch when the line was captured.
    pub timestamp_ms: u128,
    /// Which stream the line came from.
    pub stream: LogStream,
    /// The raw text of the log line.
    pub line: String,
}

#[derive(Debug)]
enum ProcessState {
    Idle,
    Starting,
    Running { pid: u32 },
    Exited { code: Option<i32> },
    Failed { error: String },
}

/// Snapshot of the current state of a managed server, suitable for JSON APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSnapshot {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub working_dir: Option<std::path::PathBuf>,
    pub autostart: bool,
    pub status: String,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
}

/// In-process supervisor for a single child HTTP server defined in the config.
#[derive(Debug)]
pub struct ManagedServer {
    cfg: ServerConfig,
    state: RwLock<ProcessState>,
    log_tx: broadcast::Sender<LogLine>,
    log_buffer: RwLock<VecDeque<LogLine>>,
    log_buffer_capacity: usize,
}

impl ManagedServer {
    pub fn new(cfg: ServerConfig, log_buffer_capacity: usize) -> Arc<Self> {
        let (log_tx, _) = broadcast::channel(1024);
        Arc::new(Self {
            cfg,
            state: RwLock::new(ProcessState::Idle),
            log_tx,
            log_buffer: RwLock::new(VecDeque::with_capacity(log_buffer_capacity)),
            log_buffer_capacity,
        })
    }

    pub fn name(&self) -> &str {
        &self.cfg.name
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.log_tx.subscribe()
    }

    pub async fn snapshot(&self) -> ServerSnapshot {
        let state = self.state.read().await;
        let (status, pid, exit_code) = match &*state {
            ProcessState::Idle => ("idle".to_string(), None, None),
            ProcessState::Starting => ("starting".to_string(), None, None),
            ProcessState::Running { pid } => ("running".to_string(), Some(*pid), None),
            ProcessState::Exited { code } => ("exited".to_string(), None, *code),
            ProcessState::Failed { error } => (format!("failed: {error}"), None, None),
        };

        ServerSnapshot {
            name: self.cfg.name.clone(),
            command: self.cfg.command.clone(),
            args: self.cfg.args.clone(),
            working_dir: self.cfg.working_dir.clone(),
            autostart: self.cfg.autostart,
            status,
            pid,
            exit_code,
        }
    }

    pub async fn recent_logs(&self, limit: usize) -> Vec<LogLine> {
        let guard = self.log_buffer.read().await;
        let len = guard.len();
        let start = len.saturating_sub(limit);
        guard.iter().skip(start).cloned().collect()
    }

    /// Spawn the configured process and begin capturing stdout/stderr.
    ///
    /// This method returns immediately after the process has been started; a
    /// background task monitors for process exit.
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        {
            let mut state = self.state.write().await;
            *state = ProcessState::Starting;
        }

        let mut cmd = Command::new(&self.cfg.command);
        cmd.args(&self.cfg.args);

        if let Some(dir) = &self.cfg.working_dir {
            cmd.current_dir(dir);
        }

        if !self.cfg.env.is_empty() {
            cmd.envs(self.cfg.env.clone());
        }

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn managed server {}", self.cfg.name))?;

        {
            let pid = child.id().unwrap_or(0);
            let mut state = self.state.write().await;
            *state = ProcessState::Running { pid };
        }

        let server = Arc::clone(self);

        // stdout task
        if let Some(stdout) = child.stdout.take() {
            Self::spawn_log_task(Arc::clone(&server), stdout, LogStream::Stdout);
        }

        // stderr task
        if let Some(stderr) = child.stderr.take() {
            Self::spawn_log_task(server.clone(), stderr, LogStream::Stderr);
        }

        // wait for exit
        tokio::spawn(async move {
            let status = child.wait().await;
            let mut state = server.state.write().await;
            match status {
                Ok(status) => {
                    *state = ProcessState::Exited {
                        code: status.code(),
                    }
                }
                Err(err) => {
                    *state = ProcessState::Failed {
                        error: err.to_string(),
                    }
                }
            }
        });

        Ok(())
    }

    fn spawn_log_task<R>(server: Arc<Self>, reader: R, stream: LogStream)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let entry = LogLine {
                    server: server.cfg.name.clone(),
                    timestamp_ms: current_epoch_ms(),
                    stream: stream.clone(),
                    line,
                };
                server.push_log(entry).await;
            }
        });
    }

    async fn push_log(&self, line: LogLine) {
        // broadcast; ignore errors if there are no subscribers
        let _ = self.log_tx.send(line.clone());

        let mut buf = self.log_buffer.write().await;
        if buf.len() == self.log_buffer_capacity {
            buf.pop_front();
        }
        buf.push_back(line);
    }
}

fn current_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
