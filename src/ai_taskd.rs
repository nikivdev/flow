use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::ai_tasks;

#[derive(Debug, Clone)]
struct CachedDiscovery {
    tasks: Vec<ai_tasks::DiscoveredAiTask>,
    refreshed_at: Instant,
}

#[derive(Debug, Clone)]
struct CachedArtifact {
    binary_path: PathBuf,
    refreshed_at: Instant,
}

#[derive(Debug, Default)]
struct TaskdState {
    discoveries: HashMap<PathBuf, CachedDiscovery>,
    artifacts: HashMap<String, CachedArtifact>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TaskdRequest {
    Ping,
    Stop,
    Run {
        project_root: String,
        selector: String,
        args: Vec<String>,
        no_cache: bool,
        #[serde(default = "default_capture_output")]
        capture_output: bool,
    },
}

fn default_capture_output() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize)]
struct TaskdResponse {
    ok: bool,
    message: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
}

pub fn start() -> Result<()> {
    if ping().is_ok() {
        println!("ai-taskd already running ({})", socket_path().display());
        return Ok(());
    }

    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let launch = format!(
        "nohup {} tasks daemon serve >/dev/null 2>&1 &",
        shell_quote(&exe.to_string_lossy())
    );
    let status = Command::new("sh")
        .arg("-lc")
        .arg(launch)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to launch ai-taskd")?;
    if !status.success() {
        bail!("failed to launch ai-taskd (status {})", status);
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if ping().is_ok() {
            println!("ai-taskd started ({})", socket_path().display());
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    bail!(
        "ai-taskd failed to start within timeout (socket: {})",
        socket_path().display()
    )
}

pub fn stop() -> Result<()> {
    let response = match send_request(&TaskdRequest::Stop) {
        Ok(response) => response,
        Err(error) => {
            let message = format!("{error:#}");
            if message.contains("Connection refused")
                || message.contains("No such file or directory")
            {
                fs::remove_file(socket_path()).ok();
                fs::remove_file(pid_path()).ok();
                println!("ai-taskd already stopped");
                return Ok(());
            }
            return Err(error);
        }
    };
    if response.ok {
        println!("{}", response.message);
        Ok(())
    } else {
        bail!(response.message)
    }
}

pub fn status() -> Result<()> {
    if ping().is_ok() {
        println!("ai-taskd: running ({})", socket_path().display());
    } else {
        println!("ai-taskd: stopped ({})", socket_path().display());
    }
    Ok(())
}

pub fn run_via_daemon(
    project_root: &Path,
    selector: &str,
    args: &[String],
    no_cache: bool,
) -> Result<()> {
    let request = TaskdRequest::Run {
        project_root: project_root.to_string_lossy().to_string(),
        selector: selector.to_string(),
        args: args.to_vec(),
        no_cache,
        capture_output: true,
    };

    let response = send_request(&request)?;
    if !response.stdout.is_empty() {
        print!("{}", response.stdout);
    }
    if !response.stderr.is_empty() {
        eprint!("{}", response.stderr);
    }

    if response.ok {
        Ok(())
    } else {
        bail!(response.message)
    }
}

pub fn serve() -> Result<()> {
    let socket = socket_path();
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if socket.exists() {
        fs::remove_file(&socket)
            .with_context(|| format!("failed to remove stale socket {}", socket.display()))?;
    }

    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind ai-taskd socket {}", socket.display()))?;

    if let Some(pid_parent) = pid_path().parent() {
        fs::create_dir_all(pid_parent)
            .with_context(|| format!("failed to create {}", pid_parent.display()))?;
    }
    fs::write(pid_path(), std::process::id().to_string())
        .with_context(|| format!("failed to write {}", pid_path().display()))?;

    let mut should_stop = false;
    let mut state = TaskdState::default();
    while !should_stop {
        let (mut stream, _) = match listener.accept() {
            Ok(tuple) => tuple,
            Err(error) => {
                eprintln!("warning: ai-taskd accept failed: {}", error);
                continue;
            }
        };
        let mut payload = Vec::new();
        if let Err(error) = stream.read_to_end(&mut payload) {
            write_error_response(
                &mut stream,
                format!("ai-taskd read request failed: {error}"),
            );
            continue;
        }

        let request: TaskdRequest = match serde_json::from_slice(&payload) {
            Ok(request) => request,
            Err(error) => {
                write_error_response(
                    &mut stream,
                    format!("ai-taskd invalid request payload: {error}"),
                );
                continue;
            }
        };
        let response = handle_request(&request, &mut state);
        if matches!(request, TaskdRequest::Stop) {
            should_stop = true;
        }

        let body = match serde_json::to_vec(&response) {
            Ok(body) => body,
            Err(error) => {
                write_error_response(
                    &mut stream,
                    format!("ai-taskd encode response failed: {error}"),
                );
                continue;
            }
        };
        if let Err(error) = stream.write_all(&body) {
            eprintln!("warning: ai-taskd write response failed: {}", error);
            continue;
        }
        stream.flush().ok();
    }

    fs::remove_file(&socket).ok();
    fs::remove_file(pid_path()).ok();
    Ok(())
}

fn handle_request(request: &TaskdRequest, state: &mut TaskdState) -> TaskdResponse {
    match request {
        TaskdRequest::Ping => TaskdResponse {
            ok: true,
            message: "pong".to_string(),
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        },
        TaskdRequest::Stop => TaskdResponse {
            ok: true,
            message: "ai-taskd stopping".to_string(),
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        },
        TaskdRequest::Run {
            project_root,
            selector,
            args,
            no_cache,
            capture_output,
        } => {
            let root = PathBuf::from(project_root);
            match run_request(state, &root, selector, args, *no_cache, *capture_output) {
                Ok((code, stdout, stderr)) => TaskdResponse {
                    ok: code == 0,
                    message: if code == 0 {
                        format!("ai task '{}' completed", selector)
                    } else {
                        format!("ai task '{}' failed with status {}", selector, code)
                    },
                    exit_code: code,
                    stdout,
                    stderr,
                },
                Err(e) => TaskdResponse {
                    ok: false,
                    message: format!("ai-taskd run failed: {e}"),
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            }
        }
    }
}

fn discovery_ttl() -> Duration {
    let ms = std::env::var("FLOW_AI_TASKD_DISCOVERY_TTL_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(750);
    Duration::from_millis(ms)
}

fn artifact_ttl() -> Duration {
    let ms = std::env::var("FLOW_AI_TASKD_ARTIFACT_TTL_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(1500);
    Duration::from_millis(ms)
}

fn get_discovered_tasks(
    state: &mut TaskdState,
    project_root: &Path,
) -> Result<(Vec<ai_tasks::DiscoveredAiTask>, bool)> {
    let key = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    if let Some(entry) = state.discoveries.get(&key)
        && entry.refreshed_at.elapsed() <= discovery_ttl()
    {
        return Ok((entry.tasks.clone(), true));
    }

    let tasks = refresh_discovery(state, &key)?;
    Ok((tasks, false))
}

fn refresh_discovery(
    state: &mut TaskdState,
    project_root: &Path,
) -> Result<Vec<ai_tasks::DiscoveredAiTask>> {
    let key = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let tasks = ai_tasks::discover_tasks(&key)?;
    state.discoveries.insert(
        key,
        CachedDiscovery {
            tasks: tasks.clone(),
            refreshed_at: Instant::now(),
        },
    );
    Ok(tasks)
}

fn run_request(
    state: &mut TaskdState,
    project_root: &Path,
    selector: &str,
    args: &[String],
    no_cache: bool,
    capture_output: bool,
) -> Result<(i32, String, String)> {
    let mut selected = ai_tasks::resolve_task_fast(project_root, selector)?;
    if selected.is_none() {
        let (tasks, from_cache) = get_discovered_tasks(state, project_root)?;
        selected = ai_tasks::select_task(&tasks, selector)?.cloned();
        if selected.is_none() && from_cache {
            // If cache was stale, refresh once and retry task selection.
            let fresh = refresh_discovery(state, project_root)?;
            selected = ai_tasks::select_task(&fresh, selector)?.cloned();
        }
    }
    let task = selected.with_context(|| format!("AI task '{}' not found", selector))?;

    if !capture_output && !no_cache {
        let status = run_cached_task_status_hot(state, project_root, &task, args)?;
        return Ok((status.code().unwrap_or(1), String::new(), String::new()));
    }

    let output = if no_cache {
        ai_tasks::run_task_via_moon_output(&task, project_root, args)?
    } else {
        run_cached_task_output_hot(state, project_root, &task, args)?
    };

    let code = output.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Ok((code, stdout, stderr))
}

fn run_cached_task_output_hot(
    state: &mut TaskdState,
    project_root: &Path,
    task: &ai_tasks::DiscoveredAiTask,
    args: &[String],
) -> Result<std::process::Output> {
    let canonical_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let key = format!("{}::{}", canonical_root.display(), task.id);

    if let Some(entry) = state.artifacts.get(&key)
        && entry.refreshed_at.elapsed() <= artifact_ttl()
        && entry.binary_path.exists()
    {
        return run_artifact_output(&entry.binary_path, &canonical_root, &task.id, args);
    }

    let artifact = ai_tasks::build_task_cached(task, &canonical_root, false)?;
    let binary_path = artifact.binary_path.clone();
    state.artifacts.insert(
        key,
        CachedArtifact {
            binary_path: binary_path.clone(),
            refreshed_at: Instant::now(),
        },
    );
    run_artifact_output(&binary_path, &canonical_root, &task.id, args)
}

fn run_cached_task_status_hot(
    state: &mut TaskdState,
    project_root: &Path,
    task: &ai_tasks::DiscoveredAiTask,
    args: &[String],
) -> Result<std::process::ExitStatus> {
    let canonical_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let key = format!("{}::{}", canonical_root.display(), task.id);

    if let Some(entry) = state.artifacts.get(&key)
        && entry.refreshed_at.elapsed() <= artifact_ttl()
        && entry.binary_path.exists()
    {
        return run_artifact_status(&entry.binary_path, &canonical_root, &task.id, args);
    }

    let artifact = ai_tasks::build_task_cached(task, &canonical_root, false)?;
    let binary_path = artifact.binary_path.clone();
    state.artifacts.insert(
        key,
        CachedArtifact {
            binary_path: binary_path.clone(),
            refreshed_at: Instant::now(),
        },
    );
    run_artifact_status(&binary_path, &canonical_root, &task.id, args)
}

fn run_artifact_output(
    binary_path: &Path,
    project_root: &Path,
    task_id: &str,
    args: &[String],
) -> Result<std::process::Output> {
    let output = Command::new(binary_path)
        .args(args)
        .current_dir(project_root)
        .env(
            "FLOW_AI_TASK_PROJECT_ROOT",
            project_root.to_string_lossy().to_string(),
        )
        .output()
        .with_context(|| {
            format!(
                "failed to run cached AI task '{}' binary {}",
                task_id,
                binary_path.display()
            )
        })?;
    Ok(output)
}

fn run_artifact_status(
    binary_path: &Path,
    project_root: &Path,
    task_id: &str,
    args: &[String],
) -> Result<std::process::ExitStatus> {
    let status = Command::new(binary_path)
        .args(args)
        .current_dir(project_root)
        .env(
            "FLOW_AI_TASK_PROJECT_ROOT",
            project_root.to_string_lossy().to_string(),
        )
        .status()
        .with_context(|| {
            format!(
                "failed to run cached AI task '{}' binary {}",
                task_id,
                binary_path.display()
            )
        })?;
    Ok(status)
}

fn ping() -> Result<()> {
    let response = send_request(&TaskdRequest::Ping)?;
    if response.ok {
        Ok(())
    } else {
        bail!(response.message)
    }
}

fn send_request(request: &TaskdRequest) -> Result<TaskdResponse> {
    let socket = socket_path();
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("failed to connect to ai-taskd at {}", socket.display()))?;
    let body = serde_json::to_vec(request).context("failed to encode ai-taskd request")?;
    stream
        .write_all(&body)
        .context("failed to write ai-taskd request")?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .context("failed to finalize ai-taskd request")?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .context("failed to read ai-taskd response")?;
    let decoded: TaskdResponse =
        serde_json::from_slice(&response).context("failed to decode ai-taskd response")?;
    Ok(decoded)
}

fn socket_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".flow")
        .join("run")
        .join("ai-taskd.sock")
}

fn pid_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".flow")
        .join("run")
        .join("ai-taskd.pid")
}

fn shell_quote(raw: &str) -> String {
    let escaped = raw.replace('\'', "'\"'\"'");
    format!("'{}'", escaped)
}

fn write_error_response(stream: &mut UnixStream, message: String) {
    let response = TaskdResponse {
        ok: false,
        message,
        exit_code: 1,
        stdout: String::new(),
        stderr: String::new(),
    };
    if let Ok(body) = serde_json::to_vec(&response) {
        let _ = stream.write_all(&body);
        let _ = stream.flush();
    }
}
