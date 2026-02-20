use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::secret_redact::redact_json_value;

const DEFAULT_SIGNAL_PATH: &str = "out/logs/flow_rl_signals.jsonl";
const DEFAULT_QUEUE_CAPACITY: usize = 8192;

struct SignalSink {
    enabled: bool,
    tx: Option<SyncSender<String>>,
    dropped: AtomicU64,
    accepted: AtomicU64,
}

static SIGNAL_SINK: OnceLock<SignalSink> = OnceLock::new();

pub fn emit(mut payload: Value) {
    let sink = SIGNAL_SINK.get_or_init(SignalSink::from_env);
    if !sink.enabled {
        return;
    }

    if !payload.is_object() {
        payload = json!({ "payload": payload });
    }

    if let Value::Object(obj) = &mut payload {
        obj.entry("schema_version".to_string())
            .or_insert_with(|| Value::String("flow_rl_event_v1".to_string()));
        obj.entry("source".to_string())
            .or_insert_with(|| Value::String("flow".to_string()));
        obj.entry("ts_unix_ms".to_string())
            .or_insert_with(|| Value::Number(now_unix_ms().into()));
    }

    redact_json_value(&mut payload);

    let Ok(line) = serde_json::to_string(&payload) else {
        return;
    };

    if let Some(tx) = sink.tx.as_ref() {
        if tx.try_send(line).is_ok() {
            sink.accepted.fetch_add(1, Ordering::Relaxed);
        } else {
            sink.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub fn stats() -> Value {
    let sink = SIGNAL_SINK.get_or_init(SignalSink::from_env);
    json!({
        "enabled": sink.enabled,
        "accepted": sink.accepted.load(Ordering::Relaxed),
        "dropped": sink.dropped.load(Ordering::Relaxed),
        "path": signal_path().display().to_string(),
    })
}

impl SignalSink {
    fn from_env() -> Self {
        if !env_enabled() {
            return Self {
                enabled: false,
                tx: None,
                dropped: AtomicU64::new(0),
                accepted: AtomicU64::new(0),
            };
        }

        let path = signal_path();
        if let Some(parent) = path.parent() {
            if fs::create_dir_all(parent).is_err() {
                return Self {
                    enabled: false,
                    tx: None,
                    dropped: AtomicU64::new(0),
                    accepted: AtomicU64::new(0),
                };
            }
        }

        let cap = std::env::var("FLOW_RL_SIGNALS_QUEUE")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(DEFAULT_QUEUE_CAPACITY)
            .max(64);
        let (tx, rx) = sync_channel::<String>(cap);

        thread::spawn(move || writer_loop(path, rx));

        Self {
            enabled: true,
            tx: Some(tx),
            dropped: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
        }
    }
}

fn env_enabled() -> bool {
    let raw = std::env::var("FLOW_RL_SIGNALS").unwrap_or_else(|_| "true".to_string());
    matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

fn signal_path() -> PathBuf {
    std::env::var("FLOW_RL_SIGNALS_PATH")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SIGNAL_PATH))
}

fn writer_loop(path: PathBuf, rx: Receiver<String>) {
    let file = OpenOptions::new().create(true).append(true).open(&path);
    let Ok(file) = file else {
        return;
    };

    let mut writer = BufWriter::new(file);
    let mut pending = 0usize;

    for line in rx {
        if writer.write_all(line.as_bytes()).is_err() {
            continue;
        }
        if writer.write_all(b"\n").is_err() {
            continue;
        }
        pending += 1;
        if pending >= 64 {
            let _ = writer.flush();
            pending = 0;
        }
    }

    let _ = writer.flush();
}

fn now_unix_ms() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(dur) => dur.as_millis() as u64,
        Err(_) => 0,
    }
}

pub fn attrs_to_object(attrs: Vec<(String, String)>) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in attrs {
        if k.is_empty() {
            continue;
        }
        out.insert(k, Value::String(v));
    }
    out
}

