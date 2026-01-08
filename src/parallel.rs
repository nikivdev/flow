//! Parallel task runner with pretty status display.

use std::io::{self, Write};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use crossterm::terminal;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, Semaphore};

// ANSI escape codes
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const BLUE: &str = "\x1b[34m";
const MAGENTA: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";
const CLEAR_LINE: &str = "\x1b[2K";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";

// Spinner frames
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_COLORS: &[&str] = &[CYAN, BLUE, MAGENTA, BLUE];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Running,
    Success,
    Failure,
    Skipped,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub label: String,
    pub command: String,
    pub status: TaskStatus,
    pub last_line: String,
    pub exit_code: Option<i32>,
    pub output: Vec<String>,
    pub duration: Option<Duration>,
}

impl Task {
    pub fn new(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            command: command.into(),
            status: TaskStatus::Pending,
            last_line: String::new(),
            exit_code: None,
            output: Vec::new(),
            duration: None,
        }
    }
}

pub struct ParallelRunner {
    tasks: Arc<Mutex<Vec<Task>>>,
    max_jobs: usize,
    fail_fast: bool,
    spinner_index: AtomicUsize,
    lines_printed: AtomicUsize,
    should_stop: AtomicBool,
    first_failure_code: Arc<Mutex<Option<i32>>>,
}

impl ParallelRunner {
    pub fn new(tasks: Vec<Task>, max_jobs: usize, fail_fast: bool) -> Self {
        Self {
            tasks: Arc::new(Mutex::new(tasks)),
            max_jobs,
            fail_fast,
            spinner_index: AtomicUsize::new(0),
            lines_printed: AtomicUsize::new(0),
            should_stop: AtomicBool::new(false),
            first_failure_code: Arc::new(Mutex::new(None)),
        }
    }

    fn get_spinner(&self) -> String {
        let idx = self.spinner_index.load(Ordering::Relaxed);
        let frame = SPINNER_FRAMES[idx % SPINNER_FRAMES.len()];
        let color = SPINNER_COLORS[idx % SPINNER_COLORS.len()];
        format!("{}{}{}", color, frame, RESET)
    }

    fn terminal_width() -> usize {
        terminal::size().map(|(w, _)| w as usize).unwrap_or(80)
    }

    fn truncate_line(text: &str, max_width: usize) -> String {
        if text.len() <= max_width {
            return text.to_string();
        }
        if max_width <= 1 {
            return text.chars().take(max_width).collect();
        }
        format!("{}…", &text[..max_width - 1])
    }

    fn strip_ansi(text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Skip escape sequence
                if chars.peek() == Some(&'[') {
                    chars.next();
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            } else {
                result.push(c);
            }
        }
        result
    }

    fn format_task_line(&self, task: &Task, label_width: usize) -> String {
        let term_width = Self::terminal_width();

        let icon = match task.status {
            TaskStatus::Pending => format!("{}○{}", DIM, RESET),
            TaskStatus::Running => self.get_spinner(),
            TaskStatus::Success => format!("{}✓{}", GREEN, RESET),
            TaskStatus::Failure => format!("{}✗{}", RED, RESET),
            TaskStatus::Skipped => format!("{}○{}", DIM, RESET),
        };

        let label = format!("{:width$}", task.label, width = label_width);
        let prefix = format!("{} {}{}{}", icon, BOLD, label, RESET);
        let prefix_len = 1 + 1 + label_width;

        match task.status {
            TaskStatus::Success => {
                if let Some(dur) = task.duration {
                    format!("{} {}({:.1}s){}", prefix, DIM, dur.as_secs_f64(), RESET)
                } else {
                    prefix
                }
            }
            TaskStatus::Failure => {
                format!(
                    "{} {}(exit {}){}",
                    prefix,
                    DIM,
                    task.exit_code.unwrap_or(-1),
                    RESET
                )
            }
            TaskStatus::Skipped => {
                format!("{} {}(skipped){}", prefix, DIM, RESET)
            }
            TaskStatus::Pending => prefix,
            TaskStatus::Running => {
                if !task.last_line.is_empty() {
                    let clean = Self::strip_ansi(&task.last_line)
                        .chars()
                        .filter(|c| c.is_ascii_graphic() || *c == ' ')
                        .collect::<String>();
                    let available = term_width.saturating_sub(prefix_len + 3);
                    if available > 0 {
                        let truncated = Self::truncate_line(&clean, available);
                        format!("{} {}{}{}", prefix, DIM, truncated, RESET)
                    } else {
                        prefix
                    }
                } else {
                    prefix
                }
            }
        }
    }

    async fn render_display(&self) {
        let tasks = self.tasks.lock().await;
        let lines_printed = self.lines_printed.load(Ordering::Relaxed);

        // Move cursor up
        if lines_printed > 0 {
            print!("\x1b[{}A", lines_printed);
        }

        let label_width = tasks.iter().map(|t| t.label.len()).max().unwrap_or(0);

        for task in tasks.iter() {
            let line = self.format_task_line(task, label_width);
            println!("{}{}", CLEAR_LINE, line);
        }

        self.lines_printed.store(tasks.len(), Ordering::Relaxed);
        let _ = io::stdout().flush();
    }

    async fn run_task(&self, task_idx: usize, semaphore: Arc<Semaphore>) {
        let _permit = semaphore.acquire().await.unwrap();

        if self.should_stop.load(Ordering::Relaxed) {
            let mut tasks = self.tasks.lock().await;
            tasks[task_idx].status = TaskStatus::Skipped;
            return;
        }

        let command = {
            let mut tasks = self.tasks.lock().await;
            tasks[task_idx].status = TaskStatus::Running;
            tasks[task_idx].command.clone()
        };

        let start = Instant::now();

        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let mut tasks = self.tasks.lock().await;
                tasks[task_idx].status = TaskStatus::Failure;
                tasks[task_idx].exit_code = Some(-1);
                tasks[task_idx]
                    .output
                    .push(format!("Failed to spawn: {}", e));
                tasks[task_idx].duration = Some(start.elapsed());

                if self.fail_fast {
                    self.should_stop.store(true, Ordering::Relaxed);
                    let mut first = self.first_failure_code.lock().await;
                    if first.is_none() {
                        *first = Some(-1);
                    }
                }
                return;
            }
        };

        // Read stdout and stderr
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let tasks_clone = Arc::clone(&self.tasks);
        let idx = task_idx;

        let stdout_handle = if let Some(stdout) = stdout {
            let tasks = Arc::clone(&tasks_clone);
            Some(tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let mut tasks = tasks.lock().await;
                    tasks[idx].output.push(format!("{}\n", line));
                    tasks[idx].last_line = line;
                }
            }))
        } else {
            None
        };

        let stderr_handle = if let Some(stderr) = stderr {
            let tasks = Arc::clone(&tasks_clone);
            Some(tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let mut tasks = tasks.lock().await;
                    tasks[idx].output.push(format!("{}\n", line));
                    if tasks[idx].last_line.is_empty() {
                        tasks[idx].last_line = line;
                    }
                }
            }))
        } else {
            None
        };

        // Wait for process
        let status = child.wait().await;
        let duration = start.elapsed();

        // Wait for output readers
        if let Some(h) = stdout_handle {
            let _ = h.await;
        }
        if let Some(h) = stderr_handle {
            let _ = h.await;
        }

        let exit_code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);

        {
            let mut tasks = self.tasks.lock().await;
            tasks[task_idx].exit_code = Some(exit_code);
            tasks[task_idx].duration = Some(duration);

            if exit_code == 0 {
                tasks[task_idx].status = TaskStatus::Success;
            } else {
                tasks[task_idx].status = TaskStatus::Failure;
                if self.fail_fast {
                    self.should_stop.store(true, Ordering::Relaxed);
                }
                let mut first = self.first_failure_code.lock().await;
                if first.is_none() {
                    *first = Some(exit_code);
                }
            }
        }
    }

    pub async fn run(self: Arc<Self>) -> i32 {
        // Hide cursor
        print!("{}", HIDE_CURSOR);
        let _ = io::stdout().flush();

        let semaphore = Arc::new(Semaphore::new(self.max_jobs));
        let task_count = self.tasks.lock().await.len();

        // Spawn all tasks
        let mut handles = Vec::new();
        for i in 0..task_count {
            let sem = Arc::clone(&semaphore);
            let runner = Arc::clone(&self);
            handles.push(tokio::spawn(async move {
                runner.run_task(i, sem).await;
            }));
        }

        // Spinner loop
        let spinner_handle = {
            let runner = Arc::clone(&self);
            tokio::spawn(async move {
                loop {
                    if runner.should_stop.load(Ordering::Relaxed) {
                        let tasks = runner.tasks.lock().await;
                        if tasks.iter().all(|t| {
                            matches!(
                                t.status,
                                TaskStatus::Success | TaskStatus::Failure | TaskStatus::Skipped
                            )
                        }) {
                            break;
                        }
                    }

                    runner.spinner_index.fetch_add(1, Ordering::Relaxed);
                    runner.render_display().await;
                    tokio::time::sleep(Duration::from_millis(80)).await;

                    let tasks = runner.tasks.lock().await;
                    if tasks.iter().all(|t| {
                        matches!(
                            t.status,
                            TaskStatus::Success | TaskStatus::Failure | TaskStatus::Skipped
                        )
                    }) {
                        break;
                    }
                }
            })
        };

        // Wait for all tasks
        for h in handles {
            let _ = h.await;
        }

        self.should_stop.store(true, Ordering::Relaxed);
        let _ = spinner_handle.await;

        // Final render
        self.render_display().await;

        // Print failures
        let tasks = self.tasks.lock().await;
        let failed: Vec<_> = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Failure)
            .collect();

        if !failed.is_empty() {
            println!();
            for task in failed {
                println!(
                    "{}{}━━━ {} (exit {}) ━━━{}",
                    RED,
                    BOLD,
                    task.label,
                    task.exit_code.unwrap_or(-1),
                    RESET
                );
                let output = task.output.join("");
                if !output.trim().is_empty() {
                    print!("{}", output);
                }
                println!();
            }
        }

        // Show cursor
        print!("{}", SHOW_CURSOR);
        let _ = io::stdout().flush();

        self.first_failure_code.lock().await.unwrap_or(0)
    }
}

/// Run tasks in parallel with pretty output.
pub async fn run_parallel(
    tasks: Vec<(&str, &str)>,
    max_jobs: usize,
    fail_fast: bool,
) -> Result<()> {
    if tasks.is_empty() {
        bail!("No tasks specified");
    }

    let tasks: Vec<Task> = tasks
        .into_iter()
        .map(|(label, cmd)| Task::new(label, cmd))
        .collect();

    let runner = Arc::new(ParallelRunner::new(tasks, max_jobs, fail_fast));
    let exit_code = runner.run().await;

    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// CLI entry point for `f parallel`.
pub fn run(cmd: crate::cli::ParallelCommand) -> Result<()> {
    use tokio::runtime::Runtime;

    if cmd.tasks.is_empty() {
        bail!("No tasks specified. Usage: f parallel 'echo hello' 'echo world' or 'label:command'");
    }

    // Parse tasks: either "label:command" or just "command" (auto-labeled)
    let tasks: Vec<(String, String)> = cmd
        .tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            if let Some((label, command)) = t.split_once(':') {
                (label.to_string(), command.to_string())
            } else {
                // Auto-generate label from command or use index
                let label = t
                    .split_whitespace()
                    .next()
                    .unwrap_or(&format!("task{}", i + 1))
                    .to_string();
                (label, t.to_string())
            }
        })
        .collect();

    let max_jobs = cmd.jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    });

    let rt = Runtime::new()?;
    rt.block_on(async {
        let task_refs: Vec<(&str, &str)> = tasks
            .iter()
            .map(|(l, c)| (l.as_str(), c.as_str()))
            .collect();
        run_parallel(task_refs, max_jobs, cmd.fail_fast).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_parallel_success() {
        let tasks = vec![
            Task::new("echo1", "echo hello"),
            Task::new("echo2", "echo world"),
        ];
        let runner = Arc::new(ParallelRunner::new(tasks, 4, false));
        let code = runner.run().await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn test_parallel_failure() {
        let tasks = vec![Task::new("fail", "exit 1"), Task::new("pass", "echo ok")];
        let runner = Arc::new(ParallelRunner::new(tasks, 4, false));
        let code = runner.run().await;
        assert_eq!(code, 1);
    }
}
