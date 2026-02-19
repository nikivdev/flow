use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use thiserror::Error;

const DEFAULT_SOCKET_PATH: &str = "/tmp/seqd.sock";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum SeqClientError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid protocol: {0}")]
    Protocol(String),
    #[error("remote error: {0}")]
    Remote(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RpcRequest {
    pub op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
}

impl RpcRequest {
    pub fn new(op: impl Into<String>) -> Self {
        Self {
            op: op.into(),
            ..Self::default()
        }
    }

    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }

    pub fn with_tool_call_id(mut self, tool_call_id: impl Into<String>) -> Self {
        self.tool_call_id = Some(tool_call_id.into());
        self
    }

    pub fn with_args_json(mut self, args: Value) -> Self {
        self.args = Some(args);
        self
    }

    pub fn with_args<T: Serialize>(mut self, args: &T) -> Result<Self, SeqClientError> {
        self.args = Some(serde_json::to_value(args)?);
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub ok: bool,
    pub op: String,
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub tool_call_id: String,
    pub ts_ms: u64,
    pub dur_us: u64,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct SeqClient {
    socket_path: PathBuf,
    stream: Mutex<UnixStream>,
}

impl SeqClient {
    pub fn connect_default() -> Result<Self, SeqClientError> {
        Self::connect(DEFAULT_SOCKET_PATH)
    }

    pub fn connect(path: impl AsRef<Path>) -> Result<Self, SeqClientError> {
        let stream = UnixStream::connect(path.as_ref())?;
        Ok(Self {
            socket_path: path.as_ref().to_path_buf(),
            stream: Mutex::new(stream),
        })
    }

    pub fn connect_with_timeout(
        path: impl AsRef<Path>,
        timeout: Duration,
    ) -> Result<Self, SeqClientError> {
        let stream = UnixStream::connect(path.as_ref())?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(Self {
            socket_path: path.as_ref().to_path_buf(),
            stream: Mutex::new(stream),
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn call(&self, request: RpcRequest) -> Result<RpcResponse, SeqClientError> {
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| SeqClientError::Protocol("socket mutex poisoned".into()))?;
        write_request(&mut stream, &request)?;
        let line = read_response_line(&mut stream)?;
        let response: RpcResponse = serde_json::from_slice(&line)?;
        Ok(response)
    }

    pub fn call_ok(&self, request: RpcRequest) -> Result<Value, SeqClientError> {
        let response = self.call(request)?;
        if response.ok {
            Ok(response.result.unwrap_or_else(|| json!({})))
        } else {
            Err(SeqClientError::Remote(
                response.error.unwrap_or_else(|| "unknown_error".to_string()),
            ))
        }
    }

    pub fn ping(&self) -> Result<RpcResponse, SeqClientError> {
        self.call(RpcRequest::new("ping"))
    }

    pub fn app_state(&self) -> Result<RpcResponse, SeqClientError> {
        self.call(RpcRequest::new("app_state"))
    }

    pub fn perf(&self) -> Result<RpcResponse, SeqClientError> {
        self.call(RpcRequest::new("perf"))
    }

    pub fn open_app(&self, name: &str) -> Result<RpcResponse, SeqClientError> {
        self.call(RpcRequest::new("open_app").with_args_json(json!({ "name": name })))
    }

    pub fn open_app_toggle(&self, name: &str) -> Result<RpcResponse, SeqClientError> {
        self.call(RpcRequest::new("open_app_toggle").with_args_json(json!({ "name": name })))
    }

    pub fn run_macro(&self, name: &str) -> Result<RpcResponse, SeqClientError> {
        self.call(RpcRequest::new("run_macro").with_args_json(json!({ "name": name })))
    }

    pub fn click(&self, x: f64, y: f64) -> Result<RpcResponse, SeqClientError> {
        self.call(RpcRequest::new("click").with_args_json(json!({ "x": x, "y": y })))
    }

    pub fn right_click(&self, x: f64, y: f64) -> Result<RpcResponse, SeqClientError> {
        self.call(RpcRequest::new("right_click").with_args_json(json!({ "x": x, "y": y })))
    }

    pub fn double_click(&self, x: f64, y: f64) -> Result<RpcResponse, SeqClientError> {
        self.call(RpcRequest::new("double_click").with_args_json(json!({ "x": x, "y": y })))
    }

    pub fn move_mouse(&self, x: f64, y: f64) -> Result<RpcResponse, SeqClientError> {
        self.call(RpcRequest::new("move").with_args_json(json!({ "x": x, "y": y })))
    }

    pub fn scroll(&self, x: f64, y: f64, dy: i32) -> Result<RpcResponse, SeqClientError> {
        self.call(RpcRequest::new("scroll").with_args_json(json!({ "x": x, "y": y, "dy": dy })))
    }

    pub fn drag(
        &self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
    ) -> Result<RpcResponse, SeqClientError> {
        self.call(
            RpcRequest::new("drag")
                .with_args_json(json!({ "x1": x1, "y1": y1, "x2": x2, "y2": y2 })),
        )
    }

    pub fn screenshot(&self, path: Option<&str>) -> Result<RpcResponse, SeqClientError> {
        let req = if let Some(path) = path {
            RpcRequest::new("screenshot").with_args_json(json!({ "path": path }))
        } else {
            RpcRequest::new("screenshot")
        };
        self.call(req)
    }
}

fn write_request(stream: &mut UnixStream, request: &RpcRequest) -> Result<(), SeqClientError> {
    let mut payload = serde_json::to_vec(request)?;
    payload.push(b'\n');
    stream.write_all(&payload)?;
    Ok(())
}

fn read_response_line(stream: &mut UnixStream) -> Result<Vec<u8>, SeqClientError> {
    let mut out = Vec::with_capacity(512);
    let mut buf = [0u8; 512];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            if out.is_empty() {
                return Err(SeqClientError::Protocol(
                    "unexpected EOF while waiting for response".to_string(),
                ));
            }
            break;
        }
        for b in &buf[..n] {
            out.push(*b);
            if *b == b'\n' {
                out.pop();
                return Ok(out);
            }
        }
        if out.len() > MAX_RESPONSE_BYTES {
            return Err(SeqClientError::Protocol(
                "response exceeded max size".to_string(),
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn test_socket_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        p.push(format!("seq_client_{tag}_{pid}_{now}.sock"));
        p
    }

    #[test]
    fn call_roundtrip_ping() {
        let path = test_socket_path("ping");
        let listener = UnixListener::bind(&path).expect("bind");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read line");
            let req: Value = serde_json::from_str(line.trim()).expect("parse req");
            assert_eq!(req["op"], "ping");
            let response = json!({
                "ok": true,
                "op": "ping",
                "request_id": "",
                "run_id": "",
                "tool_call_id": "",
                "ts_ms": 1,
                "dur_us": 2,
                "result": { "pong": true }
            });
            let mut inner = reader.into_inner();
            inner
                .write_all(format!("{}\n", response).as_bytes())
                .expect("write");
        });

        let client = SeqClient::connect(&path).expect("connect");
        let response = client.ping().expect("call");
        assert!(response.ok);
        assert_eq!(response.op, "ping");
        assert_eq!(response.result.unwrap()["pong"], true);

        server.join().expect("join");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn call_ok_surfaces_remote_error() {
        let path = test_socket_path("err");
        let listener = UnixListener::bind(&path).expect("bind");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("read line");
            let response = json!({
                "ok": false,
                "op": "open_app",
                "request_id": "r1",
                "run_id": "",
                "tool_call_id": "",
                "ts_ms": 10,
                "dur_us": 11,
                "error": "missing_name"
            });
            let mut inner = reader.into_inner();
            inner
                .write_all(format!("{}\n", response).as_bytes())
                .expect("write");
        });

        let client = SeqClient::connect(&path).expect("connect");
        let err = client
            .call_ok(RpcRequest::new("open_app"))
            .expect_err("should fail");
        match err {
            SeqClientError::Remote(s) => assert_eq!(s, "missing_name"),
            other => panic!("unexpected error: {other:?}"),
        }

        server.join().expect("join");
        let _ = fs::remove_file(path);
    }
}

