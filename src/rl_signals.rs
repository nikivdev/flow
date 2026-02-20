use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::secret_redact::redact_json_value;

const DEFAULT_SIGNAL_PATH: &str = "out/logs/flow_rl_signals.jsonl";
const DEFAULT_SEQ_MEM_PATH: &str = "~/repos/ClickHouse/ClickHouse/user_files/seq_mem.jsonl";
const DEFAULT_QUEUE_CAPACITY: usize = 8192;

struct SignalSink {
    enabled: bool,
    tx: Option<SyncSender<String>>,
    seq_mirror_enabled: bool,
    tx_seq: Option<SyncSender<String>>,
    dropped: AtomicU64,
    accepted: AtomicU64,
    dropped_seq: AtomicU64,
    accepted_seq: AtomicU64,
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

    if sink.seq_mirror_enabled
        && let Some(seq_line) = payload_to_seq_router_row(&payload)
        && let Some(tx_seq) = sink.tx_seq.as_ref()
    {
        if tx_seq.try_send(seq_line).is_ok() {
            sink.accepted_seq.fetch_add(1, Ordering::Relaxed);
        } else {
            sink.dropped_seq.fetch_add(1, Ordering::Relaxed);
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
        "seq_mirror": {
            "enabled": sink.seq_mirror_enabled,
            "accepted": sink.accepted_seq.load(Ordering::Relaxed),
            "dropped": sink.dropped_seq.load(Ordering::Relaxed),
            "path": seq_mirror_path().display().to_string(),
        }
    })
}

impl SignalSink {
    fn from_env() -> Self {
        if !env_enabled() {
            return Self {
                enabled: false,
                tx: None,
                seq_mirror_enabled: false,
                tx_seq: None,
                dropped: AtomicU64::new(0),
                accepted: AtomicU64::new(0),
                dropped_seq: AtomicU64::new(0),
                accepted_seq: AtomicU64::new(0),
            };
        }

        let path = signal_path();
        if let Some(parent) = path.parent() {
            if fs::create_dir_all(parent).is_err() {
                return Self {
                    enabled: false,
                    tx: None,
                    seq_mirror_enabled: false,
                    tx_seq: None,
                    dropped: AtomicU64::new(0),
                    accepted: AtomicU64::new(0),
                    dropped_seq: AtomicU64::new(0),
                    accepted_seq: AtomicU64::new(0),
                };
            }
        }

        let cap = std::env::var("FLOW_RL_SIGNALS_QUEUE")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(DEFAULT_QUEUE_CAPACITY)
            .max(64);
        let (tx, rx) = sync_channel::<String>(cap);

        let flush_every = flush_every();
        thread::spawn(move || writer_loop(path, rx, flush_every));

        let mut seq_mirror_enabled = false;
        let mut tx_seq = None;
        if seq_mirror_enabled_from_env() {
            let seq_path = seq_mirror_path();
            if let Some(parent) = seq_path.parent() {
                if fs::create_dir_all(parent).is_ok() {
                    let (seq_tx, seq_rx) = sync_channel::<String>(cap);
                    thread::spawn(move || writer_loop(seq_path, seq_rx, flush_every));
                    tx_seq = Some(seq_tx);
                    seq_mirror_enabled = true;
                }
            }
        }

        Self {
            enabled: true,
            tx: Some(tx),
            seq_mirror_enabled,
            tx_seq,
            dropped: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            dropped_seq: AtomicU64::new(0),
            accepted_seq: AtomicU64::new(0),
        }
    }
}

fn env_enabled() -> bool {
    let raw = std::env::var("FLOW_RL_SIGNALS").unwrap_or_else(|_| "true".to_string());
    matches!(
        raw.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn signal_path() -> PathBuf {
    std::env::var("FLOW_RL_SIGNALS_PATH")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| expand_tilde_path(&v))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SIGNAL_PATH))
}

fn seq_mirror_enabled_from_env() -> bool {
    let raw = std::env::var("FLOW_RL_SIGNALS_SEQ_MIRROR").unwrap_or_else(|_| "true".to_string());
    matches!(
        raw.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn seq_mirror_path() -> PathBuf {
    std::env::var("FLOW_RL_SIGNALS_SEQ_PATH")
        .ok()
        .or_else(|| std::env::var("SEQ_CH_MEM_PATH").ok())
        .filter(|v| !v.trim().is_empty())
        .map(|v| expand_tilde_path(&v))
        .unwrap_or_else(|| expand_tilde_path(DEFAULT_SEQ_MEM_PATH))
}

fn expand_tilde_path(value: &str) -> PathBuf {
    if value == "~"
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home);
    }
    if let Some(suffix) = value.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(suffix);
    }
    PathBuf::from(value)
}

fn writer_loop(path: PathBuf, rx: Receiver<String>, flush_every: usize) {
    let file = OpenOptions::new().create(true).append(true).open(&path);
    let Ok(file) = file else {
        return;
    };

    let mut writer = BufWriter::new(file);
    let mut pending = 0usize;
    let flush_every = flush_every.max(1);

    for line in rx {
        if writer.write_all(line.as_bytes()).is_err() {
            continue;
        }
        if writer.write_all(b"\n").is_err() {
            continue;
        }
        pending += 1;
        if pending >= flush_every {
            let _ = writer.flush();
            pending = 0;
        }
    }

    let _ = writer.flush();
}

fn flush_every() -> usize {
    std::env::var("FLOW_RL_SIGNALS_FLUSH_EVERY")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .max(1)
}

fn now_unix_ms() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(dur) => dur.as_millis() as u64,
        Err(_) => 0,
    }
}

fn payload_to_seq_router_row(payload: &Value) -> Option<String> {
    let obj = payload.as_object()?;
    let event_type = obj.get("event_type")?.as_str()?;
    if !event_type.starts_with("flow.router.") {
        return None;
    }

    let ts_ms = obj
        .get("ts_unix_ms")
        .and_then(Value::as_u64)
        .unwrap_or_else(now_unix_ms);
    let ok = obj.get("ok").and_then(Value::as_bool).unwrap_or(true);
    let session_id = obj
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("flow")
        .to_string();
    let event_id = obj
        .get("event_id")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("evt_{}", Uuid::new_v4().simple()));

    let subject = obj
        .get("subject")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let subject_json = serde_json::to_string(&subject).ok()?;
    let row = json!({
        "ts_ms": ts_ms,
        "dur_us": 0,
        "ok": ok,
        "session_id": session_id,
        "event_id": event_id,
        "content_hash": format!("flow-router-{}", Uuid::new_v4().simple()),
        "name": event_type,
        "subject": subject_json,
    });
    serde_json::to_string(&row).ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_event_maps_to_seq_row() {
        let payload = json!({
            "event_type": "flow.router.decision.v1",
            "session_id": "sess-1",
            "ok": true,
            "ts_unix_ms": 1700000000000u64,
            "subject": {
                "decision_id": "dec-1",
                "chosen_task": "ai:flow/dev-check",
            }
        });

        let line = payload_to_seq_router_row(&payload).expect("router event should map");
        let parsed: Value = serde_json::from_str(&line).expect("json line");
        assert_eq!(
            parsed.get("name").and_then(Value::as_str),
            Some("flow.router.decision.v1")
        );
        assert_eq!(
            parsed.get("session_id").and_then(Value::as_str),
            Some("sess-1")
        );
        assert_eq!(parsed.get("ok").and_then(Value::as_bool), Some(true));
        assert_eq!(
            parsed.get("ts_ms").and_then(Value::as_u64),
            Some(1700000000000u64)
        );
        assert!(
            parsed
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or("")
                .contains("\"decision_id\":\"dec-1\"")
        );
    }

    #[test]
    fn non_router_event_not_mirrored() {
        let payload = json!({
            "event_type": "everruns.run_started",
            "session_id": "sess-1",
        });
        assert!(payload_to_seq_router_row(&payload).is_none());
    }
}
