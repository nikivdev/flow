use std::env;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process;

use serde::{Deserialize, Serialize};

const MSGPACK_WIRE_PREFIX: u8 = 0xFF;

#[derive(Debug, Clone, Copy)]
enum WireProtocol {
    Json,
    Msgpack,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TaskdRequest {
    Run {
        project_root: String,
        selector: String,
        args: Vec<String>,
        no_cache: bool,
        capture_output: bool,
        include_timings: bool,
        suggested_task: Option<String>,
        override_reason: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct TaskdResponse {
    ok: bool,
    message: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
    timings: Option<RequestTimings>,
}

#[derive(Debug, Deserialize)]
struct RequestTimings {
    resolve_selector_us: u64,
    run_task_us: u64,
    total_us: u64,
    used_fast_selector: bool,
    used_cache: bool,
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
    let mut include_timings = false;
    let mut batch_stdin = false;
    let mut protocol = WireProtocol::Msgpack;
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
            "--protocol" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or("--protocol requires a value (json|msgpack)")?;
                protocol = match value.trim().to_ascii_lowercase().as_str() {
                    "json" => WireProtocol::Json,
                    "msgpack" | "mp" => WireProtocol::Msgpack,
                    other => return Err(format!("unsupported protocol '{other}'")),
                };
            }
            "--no-cache" => {
                no_cache = true;
            }
            "--capture-output" => {
                capture_output = true;
            }
            "--timings" => {
                include_timings = true;
            }
            "--batch-stdin" => {
                batch_stdin = true;
            }
            _ => break,
        }
        idx += 1;
    }

    if batch_stdin {
        return run_batch(
            &socket,
            protocol,
            &root,
            no_cache,
            capture_output,
            include_timings,
        );
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

    let response = run_once(
        &socket,
        protocol,
        &root,
        &selector,
        &trailing,
        no_cache,
        capture_output,
        include_timings,
    )?;
    print_response(&response, include_timings, &selector);
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

fn run_batch(
    socket: &PathBuf,
    protocol: WireProtocol,
    root: &str,
    no_cache: bool,
    capture_output: bool,
    include_timings: bool,
) -> Result<i32, String> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("failed to read stdin: {e}"))?;

    let mut any_failure = false;
    for (line_no, raw) in input.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let tokens = shell_words::split(line)
            .map_err(|e| format!("batch parse error at line {}: {e}", line_no + 1))?;
        if tokens.is_empty() {
            continue;
        }
        let selector = tokens[0].clone();
        let args = if tokens.len() > 1 {
            tokens[1..].to_vec()
        } else {
            Vec::new()
        };
        let response = run_once(
            socket,
            protocol,
            root,
            &selector,
            &args,
            no_cache,
            capture_output,
            include_timings,
        )?;
        print_response(&response, include_timings, &selector);
        if !response.ok {
            any_failure = true;
            eprintln!("[batch][{}] {}", selector, response.message);
        }
    }

    Ok(if any_failure { 1 } else { 0 })
}

#[allow(clippy::too_many_arguments)]
fn run_once(
    socket: &PathBuf,
    protocol: WireProtocol,
    root: &str,
    selector: &str,
    args: &[String],
    no_cache: bool,
    capture_output: bool,
    include_timings: bool,
) -> Result<TaskdResponse, String> {
    let req = TaskdRequest::Run {
        project_root: root.to_string(),
        selector: selector.to_string(),
        args: args.to_vec(),
        no_cache,
        capture_output,
        include_timings,
        suggested_task: read_optional_env("FLOW_ROUTER_SUGGESTED_TASK"),
        override_reason: read_optional_env("FLOW_ROUTER_OVERRIDE_REASON"),
    };
    send_request(socket, protocol, &req)
}

fn send_request(
    socket: &PathBuf,
    protocol: WireProtocol,
    request: &TaskdRequest,
) -> Result<TaskdResponse, String> {
    let req_bytes = encode_request(request, protocol)?;
    let mut stream = UnixStream::connect(socket)
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
    decode_response(&body)
}

fn encode_request(request: &TaskdRequest, protocol: WireProtocol) -> Result<Vec<u8>, String> {
    match protocol {
        WireProtocol::Json => {
            serde_json::to_vec(request).map_err(|e| format!("failed to encode json request: {e}"))
        }
        WireProtocol::Msgpack => {
            let mut out = vec![MSGPACK_WIRE_PREFIX];
            let encoded = rmp_serde::to_vec_named(request)
                .map_err(|e| format!("failed to encode msgpack request: {e}"))?;
            out.extend(encoded);
            Ok(out)
        }
    }
}

fn decode_response(payload: &[u8]) -> Result<TaskdResponse, String> {
    if payload.first() == Some(&MSGPACK_WIRE_PREFIX) {
        return rmp_serde::from_slice::<TaskdResponse>(&payload[1..])
            .map_err(|e| format!("failed to decode msgpack response: {e}"));
    }
    serde_json::from_slice::<TaskdResponse>(payload)
        .map_err(|e| format!("failed to decode json response: {e}"))
}

fn print_response(response: &TaskdResponse, include_timings: bool, selector: &str) {
    if !response.stdout.is_empty() {
        print!("{}", response.stdout);
    }
    if !response.stderr.is_empty() {
        eprint!("{}", response.stderr);
    }
    if include_timings && let Some(t) = &response.timings {
        eprintln!(
            "[timings][{}] resolve_us={} run_us={} total_us={} fast_selector={} cache={}",
            selector,
            t.resolve_selector_us,
            t.run_task_us,
            t.total_us,
            t.used_fast_selector,
            t.used_cache
        );
    }
}

fn default_socket_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".flow")
        .join("run")
        .join("ai-taskd.sock")
}

fn read_optional_env(key: &str) -> Option<String> {
    let raw = env::var(key).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

fn print_help() {
    println!("ai-taskd-client");
    println!("Usage:");
    println!(
        "  ai-taskd-client [--root PATH] [--socket PATH] [--protocol json|msgpack] [--no-cache] [--capture-output] [--timings] <selector> [-- <args...>]"
    );
    println!(
        "  ai-taskd-client [--root PATH] [--socket PATH] [--protocol json|msgpack] [--no-cache] [--capture-output] [--timings] --batch-stdin"
    );
    println!();
    println!("Examples:");
    println!("  ai-taskd-client ai:flow/noop");
    println!("  ai-taskd-client --protocol msgpack --timings ai:flow/noop");
    println!(
        "  printf 'ai:flow/noop\\nai:flow/dev-check -- --quick\\n' | ai-taskd-client --batch-stdin"
    );
}
