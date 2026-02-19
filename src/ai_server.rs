//! Simple AI server client for task matching.

use std::collections::HashMap;
use std::env;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::env as flow_env;

const DEFAULT_URL: &str = "http://127.0.0.1:7331";
const MAX_RETRIES: usize = 3;

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
}

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Option<ResponseMessage>,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: String,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    id: String,
}

struct AiServerConfig {
    base_url: String,
    model: String,
    token: Option<String>,
}

/// Send a prompt to the AI server and return a response.
pub fn quick_prompt(
    prompt: &str,
    model: Option<&str>,
    url: Option<&str>,
    token: Option<&str>,
) -> Result<String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        bail!("Prompt is empty.");
    }

    let cfg = resolve_ai_config(model, url, token)?;

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to create HTTP client")?;

    let endpoint = format!("{}/v1/chat/completions", cfg.base_url);

    let body = ChatRequest {
        model: cfg.model,
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
        }],
        temperature: 0.1,
    };

    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 1..=MAX_RETRIES {
        let mut req = client.post(&endpoint).json(&body);
        if let Some(token) = cfg.token.as_deref() {
            req = req.bearer_auth(token);
        }

        let resp = match req
            .send()
            .with_context(|| format!("failed to connect to AI server at {}", cfg.base_url))
        {
            Ok(resp) => resp,
            Err(err) => {
                let retryable = attempt < MAX_RETRIES;
                if retryable {
                    thread::sleep(Duration::from_millis(300 * attempt as u64));
                    last_error = Some(err);
                    continue;
                }
                return Err(err);
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            let retryable_status = status.is_server_error() || status.as_u16() == 429;
            if retryable_status && attempt < MAX_RETRIES {
                thread::sleep(Duration::from_millis(300 * attempt as u64));
                last_error = Some(anyhow::anyhow!(
                    "AI server returned retryable status {}: {}",
                    status,
                    body
                ));
                continue;
            }
            bail!("AI server returned status {}: {}", status, body);
        }

        let text_body = resp.text().context("failed to read AI server response")?;
        let parsed: ChatResponse =
            serde_json::from_str(&text_body).context("failed to parse AI server response")?;

        let text = parsed
            .choices
            .first()
            .and_then(|c| {
                c.message
                    .as_ref()
                    .map(|m| m.content.clone())
                    .or(c.text.clone())
            })
            .map(|t| t.trim().to_string())
            .unwrap_or_default();

        return Ok(text);
    }

    if let Some(err) = last_error {
        return Err(err);
    }
    bail!("AI request failed unexpectedly without a captured error.")
}

fn resolve_ai_config(
    model_override: Option<&str>,
    url_override: Option<&str>,
    token_override: Option<&str>,
) -> Result<AiServerConfig> {
    let mut resolved: HashMap<String, String> = HashMap::new();
    let mut missing = Vec::new();
    let keys = ["AI_SERVER_URL", "AI_SERVER_MODEL", "AI_SERVER_TOKEN"];

    for key in keys {
        if let Ok(value) = env::var(key) {
            if !value.trim().is_empty() {
                resolved.insert(key.to_string(), value);
                continue;
            }
        }
        missing.push(key.to_string());
    }

    if !missing.is_empty() {
        if let Ok(vars) = flow_env::fetch_personal_env_vars(&missing) {
            for (key, value) in vars {
                if !value.trim().is_empty() {
                    resolved.insert(key, value);
                }
            }
        }
    }

    let mut url = url_override
        .map(|s| s.to_string())
        .or_else(|| resolved.get("AI_SERVER_URL").cloned())
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    if url.trim().is_empty() {
        url = DEFAULT_URL.to_string();
    }
    let base_url = base_ai_url(&url);

    let token = token_override
        .map(|s| s.to_string())
        .or_else(|| resolved.get("AI_SERVER_TOKEN").cloned())
        .filter(|v| !v.trim().is_empty());

    let model = model_override
        .map(|s| s.to_string())
        .or_else(|| resolved.get("AI_SERVER_MODEL").cloned())
        .unwrap_or_default();

    let model = if model.trim().is_empty() {
        fetch_default_model(&base_url, token.as_deref())?
    } else {
        model
    };

    Ok(AiServerConfig {
        base_url,
        model,
        token,
    })
}

fn fetch_default_model(base_url: &str, token: Option<&str>) -> Result<String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to create HTTP client")?;
    let url = format!("{}/v1/models", base_url);
    let mut req = client.get(&url);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }

    let resp = req
        .send()
        .with_context(|| format!("failed to query models at {}", base_url))?;

    if !resp.status().is_success() {
        bail!(
            "AI_SERVER_MODEL not set and /v1/models failed with status {}. Set it with: f env set --personal AI_SERVER_MODEL=<model>",
            resp.status()
        );
    }

    let text_body = resp.text().context("failed to read models response")?;
    let parsed: ModelsResponse =
        serde_json::from_str(&text_body).context("failed to parse models response")?;

    let model = parsed
        .data
        .into_iter()
        .find(|m| !m.id.trim().is_empty())
        .map(|m| m.id)
        .unwrap_or_default();

    if model.trim().is_empty() {
        bail!(
            "AI_SERVER_MODEL not set and no models returned. Set it with: f env set --personal AI_SERVER_MODEL=<model>"
        );
    }

    Ok(model)
}

fn base_ai_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if let Some(idx) = trimmed.find("/v1/") {
        return trimmed[..idx].to_string();
    }
    trimmed.to_string()
}
