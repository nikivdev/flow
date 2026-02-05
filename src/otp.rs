use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use data_encoding::BASE32_NOPAD;
use hmac::{Hmac, Mac};
use reqwest::blocking::Client;
use serde::Deserialize;
use sha1::Sha1;
use sha2::{Sha256, Sha512};
use url::Url;

use crate::cli::{OtpAction, OtpCommand};
use crate::env;

#[derive(Debug, Clone)]
struct ConnectConfig {
    host: String,
    token: String,
}

#[derive(Debug, Deserialize)]
struct Vault {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct ItemSummary {
    id: String,
    title: String,
}

#[derive(Debug, Deserialize)]
struct Field {
    #[serde(rename = "type")]
    field_type: String,
    label: Option<String>,
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FullItem {
    #[allow(dead_code)]
    id: String,
    title: String,
    fields: Option<Vec<Field>>,
}

pub fn run(cmd: OtpCommand) -> Result<()> {
    match cmd.action {
        OtpAction::Get { vault, item, field } => {
            let config = load_connect_config()?;
            let code = fetch_totp(&config, &vault, &item, field.as_deref())?;
            println!("{code}");
        }
    }
    Ok(())
}

fn load_connect_config() -> Result<ConnectConfig> {
    let host = std::env::var("OP_CONNECT_HOST")
        .or_else(|_| std::env::var("OP_CONNECT_URL"))
        .map_err(|_| anyhow::anyhow!("OP_CONNECT_HOST is not set"))?;

    let token = std::env::var("OP_CONNECT_TOKEN").ok().or_else(|| {
        env::fetch_personal_env_vars(&["OP_CONNECT_TOKEN".to_string()])
            .ok()
            .and_then(|vars| vars.get("OP_CONNECT_TOKEN").cloned())
    });

    let Some(token) = token else {
        bail!("OP_CONNECT_TOKEN not found in env or Flow env store");
    };

    Ok(ConnectConfig { host, token })
}

fn fetch_totp(
    config: &ConnectConfig,
    vault_ref: &str,
    item_ref: &str,
    field_label: Option<&str>,
) -> Result<String> {
    let client = Client::builder()
        .build()
        .context("failed to build HTTP client")?;

    let vault_id = resolve_vault_id(&client, config, vault_ref)?;
    let item_id = resolve_item_id(&client, config, &vault_id, item_ref)?;
    let item = fetch_item(&client, config, &vault_id, &item_id)?;

    let totp_uri = extract_totp_uri(&item, field_label)?;
    compute_totp(&totp_uri)
}

fn resolve_vault_id(client: &Client, config: &ConnectConfig, vault_ref: &str) -> Result<String> {
    let url = format!("{}/v1/vaults", config.host.trim_end_matches('/'));
    let vaults: Vec<Vault> = client
        .get(url)
        .bearer_auth(&config.token)
        .send()
        .context("failed to list 1Password vaults")?
        .error_for_status()
        .context("1Password connect returned an error for vault list")?
        .json()
        .context("failed to parse vault list")?;

    if let Some(vault) = vaults
        .iter()
        .find(|v| v.id == vault_ref || v.name == vault_ref)
    {
        return Ok(vault.id.clone());
    }

    bail!("vault not found: {}", vault_ref);
}

fn resolve_item_id(
    client: &Client,
    config: &ConnectConfig,
    vault_id: &str,
    item_ref: &str,
) -> Result<String> {
    let url = format!(
        "{}/v1/vaults/{}/items",
        config.host.trim_end_matches('/'),
        vault_id
    );
    let items: Vec<ItemSummary> = client
        .get(url)
        .bearer_auth(&config.token)
        .send()
        .context("failed to list 1Password items")?
        .error_for_status()
        .context("1Password connect returned an error for item list")?
        .json()
        .context("failed to parse item list")?;

    if let Some(item) = items
        .iter()
        .find(|i| i.id == item_ref || i.title == item_ref)
    {
        return Ok(item.id.clone());
    }

    bail!("item not found: {}", item_ref);
}

fn fetch_item(
    client: &Client,
    config: &ConnectConfig,
    vault_id: &str,
    item_id: &str,
) -> Result<FullItem> {
    let url = format!(
        "{}/v1/vaults/{}/items/{}",
        config.host.trim_end_matches('/'),
        vault_id,
        item_id
    );
    let item: FullItem = client
        .get(url)
        .bearer_auth(&config.token)
        .send()
        .context("failed to fetch 1Password item")?
        .error_for_status()
        .context("1Password connect returned an error for item fetch")?
        .json()
        .context("failed to parse item")?;
    Ok(item)
}

fn extract_totp_uri(item: &FullItem, field_label: Option<&str>) -> Result<String> {
    let fields = item.fields.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "item '{}' has no fields; expected a TOTP field",
            item.title
        )
    })?;

    let mut candidates: Vec<&Field> = fields
        .iter()
        .filter(|field| field.field_type.eq_ignore_ascii_case("TOTP"))
        .collect();

    if let Some(label) = field_label {
        let label_lower = label.to_lowercase();
        candidates = candidates
            .into_iter()
            .filter(|field| {
                field
                    .label
                    .as_ref()
                    .map(|l| l.to_lowercase() == label_lower)
                    .unwrap_or(false)
            })
            .collect();
    }

    let field = candidates
        .first()
        .ok_or_else(|| anyhow::anyhow!("no TOTP field found in item '{}'", item.title))?;

    let value = field
        .value
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("TOTP field in '{}' has no value", item.title))?;

    Ok(value.clone())
}

fn compute_totp(uri: &str) -> Result<String> {
    if !uri.starts_with("otpauth://") {
        return compute_totp_from_secret(uri, 30, 6, "SHA1");
    }

    let url = Url::parse(uri).context("failed to parse otpauth URI")?;
    if url.scheme() != "otpauth" {
        bail!("unsupported OTP URI scheme: {}", url.scheme());
    }

    let mut secret: Option<String> = None;
    let mut digits: u32 = 6;
    let mut period: u64 = 30;
    let mut algorithm = "SHA1".to_string();

    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "secret" => secret = Some(value.to_string()),
            "digits" => digits = value.parse::<u32>().unwrap_or(6),
            "period" => period = value.parse::<u64>().unwrap_or(30),
            "algorithm" => algorithm = value.to_string(),
            _ => {}
        }
    }

    let secret = secret.ok_or_else(|| anyhow::anyhow!("otpauth URI missing secret"))?;
    compute_totp_from_secret(&secret, period, digits, &algorithm)
}

fn compute_totp_from_secret(secret: &str, period: u64, digits: u32, algorithm: &str) -> Result<String> {
    let key = decode_base32(secret)?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before Unix epoch")?
        .as_secs();
    let counter = timestamp / period;

    let msg = counter.to_be_bytes();
    let algo_upper = algorithm.to_uppercase();

    let hash = if algo_upper == "SHA256" {
        hmac_sha256(&key, &msg)
    } else if algo_upper == "SHA512" {
        hmac_sha512(&key, &msg)
    } else {
        hmac_sha1(&key, &msg)
    };

    let offset = (hash[hash.len() - 1] & 0x0f) as usize;
    let slice = &hash[offset..offset + 4];
    let mut code = ((u32::from(slice[0]) & 0x7f) << 24)
        | (u32::from(slice[1]) << 16)
        | (u32::from(slice[2]) << 8)
        | u32::from(slice[3]);
    let modulo = 10u32.pow(digits);
    code %= modulo;

    Ok(format!("{:0width$}", code, width = digits as usize))
}

fn decode_base32(secret: &str) -> Result<Vec<u8>> {
    let normalized = secret.trim().replace(' ', "").to_uppercase();
    BASE32_NOPAD
        .decode(normalized.as_bytes())
        .context("failed to decode base32 secret")
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

fn hmac_sha512(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha512>::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}
