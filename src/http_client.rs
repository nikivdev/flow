use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::blocking::Client;

static BLOCKING_CLIENTS: OnceLock<Mutex<HashMap<u64, Client>>> = OnceLock::new();

fn timeout_key(timeout: Duration) -> u64 {
    timeout.as_millis().min(u64::MAX as u128) as u64
}

/// Reuse blocking reqwest clients by timeout bucket to avoid repeated TLS/client init.
pub fn blocking_with_timeout(timeout: Duration) -> Result<Client> {
    let clients = BLOCKING_CLIENTS.get_or_init(|| Mutex::new(HashMap::new()));
    let key = timeout_key(timeout);
    let mut guard = clients
        .lock()
        .map_err(|_| anyhow::anyhow!("http client cache mutex poisoned"))?;

    if let Some(client) = guard.get(&key) {
        return Ok(client.clone());
    }

    let client = Client::builder()
        .timeout(timeout)
        .build()
        .with_context(|| format!("failed to build http client with timeout {:?}", timeout))?;
    guard.insert(key, client.clone());
    Ok(client)
}
