use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl RpcRequest {
    pub fn new(op: impl Into<String>) -> Self {
        Self {
            op: op.into(),
            args: None,
            request_id: None,
            run_id: None,
            tool_call_id: None,
        }
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
    #[serde(default)]
    pub ts_ms: u64,
    #[serde(default)]
    pub dur_us: u64,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

#[cfg(unix)]
pub struct SeqClient {
    reader: BufReader<std::os::unix::net::UnixStream>,
    read_buf: Vec<u8>,
}

#[cfg(unix)]
impl SeqClient {
    pub fn connect_with_timeout(socket_path: impl AsRef<Path>, timeout: Duration) -> Result<Self> {
        let path = socket_path.as_ref();
        let stream = std::os::unix::net::UnixStream::connect(path)
            .with_context(|| format!("failed to connect to seqd socket {}", path.display()))?;
        stream
            .set_read_timeout(Some(timeout))
            .context("failed to set seqd socket read timeout")?;
        stream
            .set_write_timeout(Some(timeout))
            .context("failed to set seqd socket write timeout")?;
        Ok(Self {
            reader: BufReader::new(stream),
            read_buf: Vec::with_capacity(1024),
        })
    }

    pub fn call(&mut self, req: &RpcRequest) -> Result<RpcResponse> {
        let mut encoded = serde_json::to_vec(req).context("failed to encode seqd rpc request")?;
        encoded.push(b'\n');
        let stream = self.reader.get_mut();
        stream
            .write_all(&encoded)
            .context("failed to write seqd rpc request")?;
        stream.flush().context("failed to flush seqd rpc request")?;

        self.read_buf.clear();
        self.reader
            .read_until(b'\n', &mut self.read_buf)
            .context("failed to read seqd rpc response")?;

        if self.read_buf.last() == Some(&b'\n') {
            self.read_buf.pop();
        }

        if self.read_buf.is_empty() {
            bail!("empty response from seqd");
        }
        if self.read_buf.len() > 1_000_000 {
            bail!("seqd rpc response exceeded 1MB line limit");
        }

        let resp: RpcResponse = crate::json_parse::parse_json_bytes_in_place(&mut self.read_buf)
            .context("failed to decode seqd rpc response json")?;
        Ok(resp)
    }
}

#[cfg(not(unix))]
pub struct SeqClient;

#[cfg(not(unix))]
impl SeqClient {
    pub fn connect_with_timeout(
        _socket_path: impl AsRef<Path>,
        _timeout: Duration,
    ) -> Result<Self> {
        bail!("seq client is only supported on unix platforms")
    }

    pub fn call(&mut self, _req: &RpcRequest) -> Result<RpcResponse> {
        bail!("seq client is only supported on unix platforms")
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use tempfile::tempdir;

    #[test]
    fn rpc_roundtrip_line_delimited() -> Result<()> {
        let dir = tempdir()?;
        let socket_path = dir.path().join("seqd.sock");
        let listener = UnixListener::bind(&socket_path)?;

        let server = thread::spawn(move || -> Result<()> {
            let (mut stream, _) = listener.accept()?;
            let mut got = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = stream.read(&mut byte)?;
                if n == 0 || byte[0] == b'\n' {
                    break;
                }
                got.push(byte[0]);
            }
            let req_text = String::from_utf8(got).context("req not utf8")?;
            let req: RpcRequest = serde_json::from_str(&req_text).context("req not json")?;
            if req.op != "ping" {
                bail!("unexpected op");
            }

            let reply = serde_json::json!({
                "ok": true,
                "op": "ping",
                "request_id": req.request_id.unwrap_or_default(),
                "run_id": req.run_id.unwrap_or_default(),
                "tool_call_id": req.tool_call_id.unwrap_or_default(),
                "ts_ms": 1,
                "dur_us": 2,
                "result": {"pong": true}
            })
            .to_string();
            stream.write_all(reply.as_bytes())?;
            stream.write_all(b"\n")?;
            Ok(())
        });

        let mut client = SeqClient::connect_with_timeout(&socket_path, Duration::from_secs(2))?;
        let mut req = RpcRequest::new("ping");
        req.request_id = Some("abc".to_string());
        let resp = client.call(&req)?;
        assert!(resp.ok);
        assert_eq!(resp.op, "ping");
        assert_eq!(resp.request_id, "abc");
        server.join().expect("server thread panicked")?;
        Ok(())
    }
}
