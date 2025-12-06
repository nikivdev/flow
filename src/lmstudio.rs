//! Simple LM Studio API client for task matching.

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

const DEFAULT_PORT: u16 = 1234;
const DEFAULT_MODEL: &str = "qwen3-8b";

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
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: String,
}

/// Send a prompt to LM Studio and get a response.
pub fn quick_prompt(prompt: &str, model: Option<&str>, port: Option<u16>) -> Result<String> {
    let prompt = prompt.trim();
    let model = model.unwrap_or(DEFAULT_MODEL);
    let port = port.unwrap_or(DEFAULT_PORT);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to create HTTP client")?;

    let url = format!("http://localhost:{port}/v1/chat/completions");

    let body = ChatRequest {
        model: model.to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
        }],
        temperature: 0.1, // Low temperature for deterministic task matching
    };

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .with_context(|| format!("failed to connect to LM Studio at localhost:{port}"))?;

    if !resp.status().is_success() {
        anyhow::bail!(
            "LM Studio returned status {}: {}",
            resp.status(),
            resp.text().unwrap_or_default()
        );
    }

    let text_body = resp.text().context("failed to read LM Studio response")?;
    let parsed: ChatResponse =
        serde_json::from_str(&text_body).context("failed to parse LM Studio response")?;

    let text = parsed
        .choices
        .first()
        .and_then(|c| c.message.as_ref())
        .map(|m| m.content.trim().to_string())
        .unwrap_or_default();

    Ok(text)
}

/// Check if LM Studio is running and accessible.
pub fn is_available(port: Option<u16>) -> bool {
    let port = port.unwrap_or(DEFAULT_PORT);
    let client = match Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    let url = format!("http://localhost:{port}/v1/models");
    client.get(&url).send().map(|r| r.status().is_success()).unwrap_or(false)
}
