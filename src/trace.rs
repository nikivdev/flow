use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose};
use notify::{RecursiveMode, Watcher};

use crate::cli::TraceOpts;

const META_DIR_SUFFIX: &str = ".flow/tty-meta";
const TTY_LOG_DIR_SUFFIX: &str = ".flow/tmux-logs";

pub fn run(opts: TraceOpts) -> Result<()> {
    if opts.last_command {
        return print_last_command();
    }
    stream_operations()
}

fn stream_operations() -> Result<()> {
    let meta_dir = meta_dir();
    if !meta_dir.exists() {
        bail!(
            "no meta dir at {}; enable trace_terminal_io and open a new terminal",
            meta_dir.display()
        );
    }

    let mut positions = HashMap::new();
    bootstrap_existing(&meta_dir, &mut positions)?;

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("failed to start watcher on tty meta dir")?;
    watcher
        .watch(&meta_dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", meta_dir.display()))?;

    println!("# streaming command events (Ctrl+C to stop)");
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(event)) => {
                for path in event.paths {
                    if path.extension().and_then(|s| s.to_str()) != Some("log") {
                        continue;
                    }
                    let _ = print_new_lines(&path, &mut positions);
                }
            }
            Ok(Err(err)) => {
                eprintln!("watch error: {err}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // poll for new files
                let _ = bootstrap_existing(&meta_dir, &mut positions);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Ok(())
}

fn bootstrap_existing(meta_dir: &Path, positions: &mut HashMap<PathBuf, u64>) -> Result<()> {
    for entry in
        fs::read_dir(meta_dir).with_context(|| format!("failed to read {}", meta_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("log") {
            continue;
        }
        if !positions.contains_key(&path) {
            positions.insert(path.clone(), 0);
            print_new_lines(&path, positions)?;
        }
    }
    Ok(())
}

fn print_new_lines(path: &Path, positions: &mut HashMap<PathBuf, u64>) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let pos = positions.entry(path.to_path_buf()).or_insert(0);
    file.seek(SeekFrom::Start(*pos))
        .with_context(|| format!("failed to seek {}", path.display()))?;

    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    while reader.read_line(&mut buf)? != 0 {
        *pos += buf.len() as u64;
        if let Some(evt) = parse_meta_line(buf.trim_end()) {
            println!("{}", format_event(evt, path));
        }
        buf.clear();
    }

    Ok(())
}

fn print_last_command() -> Result<()> {
    let meta_dir = meta_dir();
    let tty_dir = tty_dir();
    if !meta_dir.exists() {
        bail!(
            "no meta data found at {}; enable trace_terminal_io and run commands inside tmux",
            meta_dir.display()
        );
    }

    if !tty_dir.exists() {
        bail!(
            "no tmux logs at {}; ensure shells run inside tmux",
            tty_dir.display()
        );
    }

    let (last_evt, start_map) = latest_event(&meta_dir)?;
    let Some(evt) = last_evt else {
        bail!("no commands recorded yet");
    };
    let cmd = start_map.get(&evt.id).cloned();

    let output = extract_command_output(&evt.id, &tty_dir)
        .with_context(|| format!("failed to find output for command {}", evt.id))?;

    if let Some(start) = cmd {
        println!(
            "command: {}",
            start.cmd.unwrap_or_else(|| "<unknown>".to_string())
        );
        if let Some(cwd) = start.cwd {
            println!("cwd: {cwd}");
        }
    } else {
        println!("command: <unknown>");
    }
    if let Some(status) = evt.status {
        println!("status: {status}");
    }
    println!("--- output ---");
    print!("{output}");
    Ok(())
}

fn extract_command_output(id: &str, tty_dir: &Path) -> Result<String> {
    let start_marker = format!("flow-cmd-start;{id}");
    let end_marker = format!("flow-cmd-end;{id}");

    for entry in
        fs::read_dir(tty_dir).with_context(|| format!("failed to read {}", tty_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("log") {
            continue;
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read tty log {}", path.display()))?;

        if let Some(start_pos) = content.find(&start_marker) {
            let after_start = content[start_pos..]
                .find('\x07')
                .map(|idx| start_pos + idx + 1)
                .unwrap_or(start_pos);
            if let Some(end_pos) = content[after_start..].find(&end_marker) {
                let end_idx = after_start + end_pos;
                let slice = &content[after_start..end_idx];
                return Ok(slice.trim_matches(|c| c == '\n' || c == '\r').to_string());
            }
        }
    }

    bail!("command id {id} not found in tty logs; ensure command ran inside tmux")
}

#[derive(Clone)]
struct MetaEvent {
    ts: String,
    id: String,
    kind: MetaKind,
    cmd: Option<String>,
    cwd: Option<String>,
    status: Option<i32>,
}

#[derive(Clone)]
enum MetaKind {
    Start,
    End,
}

fn parse_meta_line(line: &str) -> Option<MetaEvent> {
    let mut parts = line.split_whitespace();
    let kind = parts.next()?;
    let ts = parts.next()?.to_string();

    match kind {
        "start" => {
            let id = parts.next()?.to_string();
            let cwd_b64 = parts.next().unwrap_or("");
            let cmd_b64 = parts.next().unwrap_or("");
            Some(MetaEvent {
                ts,
                id,
                kind: MetaKind::Start,
                cwd: decode_b64(cwd_b64),
                cmd: decode_b64(cmd_b64),
                status: None,
            })
        }
        "end" => {
            let id = parts.next()?.to_string();
            let status = parts.next().and_then(|s| s.parse::<i32>().ok());
            Some(MetaEvent {
                ts,
                id,
                kind: MetaKind::End,
                cmd: None,
                cwd: None,
                status,
            })
        }
        _ => None,
    }
}

fn format_event(evt: MetaEvent, path: &Path) -> String {
    match evt.kind {
        MetaKind::Start => format!(
            "[{} {}] start {} (cwd: {})",
            path.file_stem().and_then(|s| s.to_str()).unwrap_or("pane"),
            evt.ts,
            evt.cmd.unwrap_or_else(|| "<unknown>".to_string()),
            evt.cwd.unwrap_or_else(|| "?".to_string())
        ),
        MetaKind::End => format!(
            "[{} {}] end status={}",
            path.file_stem().and_then(|s| s.to_str()).unwrap_or("pane"),
            evt.ts,
            evt.status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "?".to_string())
        ),
    }
}

fn latest_event(meta_dir: &Path) -> Result<(Option<MetaEvent>, HashMap<String, MetaEvent>)> {
    let mut last: Option<MetaEvent> = None;
    let mut starts: HashMap<String, MetaEvent> = HashMap::new();

    for entry in
        fs::read_dir(meta_dir).with_context(|| format!("failed to read {}", meta_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("log") {
            continue;
        }
        let file =
            File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            if let Some(evt) = parse_meta_line(&line) {
                if matches!(evt.kind, MetaKind::Start) {
                    starts.insert(evt.id.clone(), evt.clone());
                }
                if last.as_ref().map_or(true, |prev| evt.ts > prev.ts) {
                    last = Some(evt);
                }
            }
        }
    }

    Ok((last, starts))
}

fn decode_b64(input: &str) -> Option<String> {
    general_purpose::STANDARD
        .decode(input.as_bytes())
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

fn meta_dir() -> PathBuf {
    home_dir().join(META_DIR_SUFFIX)
}

fn tty_dir() -> PathBuf {
    home_dir().join(TTY_LOG_DIR_SUFFIX)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
