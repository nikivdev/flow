use std::env;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TaskdRequest {
    Run {
        project_root: String,
        selector: String,
        args: Vec<String>,
        no_cache: bool,
        capture_output: bool,
    },
}

#[derive(Debug, Deserialize)]
struct TaskdResponse {
    ok: bool,
    message: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
}

fn main() {
    match run() {
        Ok(code) => process::exit(code),
        Err(msg) => {
            eprintln!("{msg}");
            process::exit(1);
        }
    }
}

fn run() -> Result<i32, String> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(0);
    }

    let mut root = env::current_dir()
        .map_err(|e| format!("failed to resolve cwd: {e}"))?
        .to_string_lossy()
        .to_string();
    let mut no_cache = false;
    let mut capture_output = false;
    let mut socket = default_socket_path();

    let mut idx = 0usize;
    while idx < args.len() {
        let arg = args[idx].clone();
        match arg.as_str() {
            "--root" => {
                idx += 1;
                let value = args.get(idx).ok_or("--root requires a value")?;
                root = value.clone();
            }
            "--socket" => {
                idx += 1;
                let value = args.get(idx).ok_or("--socket requires a value")?;
                socket = PathBuf::from(value);
            }
            "--no-cache" => {
                no_cache = true;
            }
            "--capture-output" => {
                capture_output = true;
            }
            _ => break,
        }
        idx += 1;
    }

    if idx >= args.len() {
        return Err("missing selector".to_string());
    }
    let selector = args[idx].clone();
    let trailing = if idx + 1 < args.len() {
        if args[idx + 1] == "--" {
            args[(idx + 2)..].to_vec()
        } else {
            args[(idx + 1)..].to_vec()
        }
    } else {
        Vec::new()
    };

    let req = TaskdRequest::Run {
        project_root: root,
        selector,
        args: trailing,
        no_cache,
        capture_output,
    };
    let req_bytes =
        serde_json::to_vec(&req).map_err(|e| format!("failed to encode request: {e}"))?;

    let mut stream = UnixStream::connect(&socket)
        .map_err(|e| format!("failed to connect to {}: {e}", socket.display()))?;
    stream
        .write_all(&req_bytes)
        .map_err(|e| format!("failed to write request: {e}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("failed to finalize request: {e}"))?;

    let mut body = Vec::new();
    stream
        .read_to_end(&mut body)
        .map_err(|e| format!("failed to read response: {e}"))?;
    let response: TaskdResponse =
        serde_json::from_slice(&body).map_err(|e| format!("failed to decode response: {e}"))?;

    if !response.stdout.is_empty() {
        print!("{}", response.stdout);
    }
    if !response.stderr.is_empty() {
        eprint!("{}", response.stderr);
    }
    if response.ok {
        return Ok(0);
    }
    eprintln!("{}", response.message);
    Ok(if response.exit_code == 0 {
        1
    } else {
        response.exit_code
    })
}

fn default_socket_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".flow")
        .join("run")
        .join("ai-taskd.sock")
}

fn print_help() {
    println!("ai-taskd-client");
    println!("Usage:");
    println!(
        "  ai-taskd-client [--root PATH] [--socket PATH] [--no-cache] [--capture-output] <selector> [-- <args...>]"
    );
    println!();
    println!("Examples:");
    println!("  ai-taskd-client ai:flow/noop");
    println!("  ai-taskd-client --root ~/code/flow ai:flow/bench-cli -- --iterations 50");
}
