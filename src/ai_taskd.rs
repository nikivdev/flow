use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::ai_tasks;

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
    },
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
            if message.contains("Connection refused") || message.contains("No such file or directory")
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
            write_error_response(&mut stream, format!("ai-taskd read request failed: {error}"));
            continue;
        }

        let request: TaskdRequest = match serde_json::from_slice(&payload) {
            Ok(request) => request,
            Err(error) => {
                write_error_response(&mut stream, format!("ai-taskd invalid request payload: {error}"));
                continue;
            }
        };
        let response = handle_request(&request);
        if matches!(request, TaskdRequest::Stop) {
            should_stop = true;
        }

        let body = match serde_json::to_vec(&response) {
            Ok(body) => body,
            Err(error) => {
                write_error_response(&mut stream, format!("ai-taskd encode response failed: {error}"));
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

fn handle_request(request: &TaskdRequest) -> TaskdResponse {
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
        } => {
            let root = PathBuf::from(project_root);
            match run_request(&root, selector, args, *no_cache) {
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

fn run_request(
    project_root: &Path,
    selector: &str,
    args: &[String],
    no_cache: bool,
) -> Result<(i32, String, String)> {
    let tasks = ai_tasks::discover_tasks(project_root)?;
    let task = ai_tasks::select_task(&tasks, selector)?
        .with_context(|| format!("AI task '{}' not found", selector))?;

    let output = if no_cache {
        ai_tasks::run_task_via_moon_output(task, project_root, args)?
    } else {
        ai_tasks::run_task_cached_output(task, project_root, args)?
    };

    let code = output.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Ok((code, stdout, stderr))
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
