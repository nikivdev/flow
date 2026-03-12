use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    cli::{
        UrlAction, UrlCommand, UrlCrawlOpts, UrlCrawlSource, UrlInspectOpts, UrlInspectProvider,
    },
    config, env as flow_env, http_client, project_snapshot,
};

const DEFAULT_EXCERPT_CHARS: usize = 420;
const DEFAULT_DIRECT_ACCEPT: &str = "text/markdown, text/html;q=0.9, text/plain;q=0.8, */*;q=0.1";
const CLOUDFLARE_API_BASE: &str = "https://api.cloudflare.com/client/v4";

#[derive(Debug, Clone, Default)]
struct UrlInspectSettings {
    scraper_base_url: Option<String>,
    scraper_api_key: Option<String>,
    cache_ttl_hours: Option<f64>,
    allow_direct_fallback: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UrlInspectResult {
    pub reference: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UrlCrawlResult {
    pub reference: String,
    pub provider: String,
    pub job_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_seconds_used: Option<f64>,
    pub render: bool,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub records: Vec<UrlCrawlRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UrlCrawlRecord {
    pub url: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CloudflareEnvelope {
    success: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    errors: Vec<CloudflareError>,
}

#[derive(Debug, Deserialize)]
struct CloudflareError {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ScrapeResult {
    success: bool,
    #[serde(default)]
    final_url: Option<String>,
    #[serde(default)]
    status_code: Option<u16>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    text_excerpt: Option<String>,
    #[serde(default)]
    cache_hit: Option<bool>,
    #[serde(default)]
    error: Option<String>,
}

pub fn run(cmd: UrlCommand) -> Result<()> {
    match cmd.action {
        UrlAction::Inspect(opts) => inspect(opts),
        UrlAction::Crawl(opts) => crawl(opts),
    }
}

pub fn inspect_compact(url: &str, cwd: &Path) -> Result<String> {
    let settings = load_url_inspect_settings(cwd);
    let timeout = timeout_from_secs(12.0)?;
    let opts = UrlInspectOpts {
        url: url.to_string(),
        json: false,
        full: false,
        provider: UrlInspectProvider::Auto,
        timeout_s: 12.0,
    };
    let result = inspect_url(&opts, &settings, timeout)?;
    Ok(render_compact_result(&result))
}

fn inspect(opts: UrlInspectOpts) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let settings = load_url_inspect_settings(&cwd);
    let timeout = timeout_from_secs(opts.timeout_s)?;
    let result = inspect_url(&opts, &settings, timeout)?;
    print_result(&result, opts.json, opts.full)
}

fn crawl(opts: UrlCrawlOpts) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let settings = load_url_inspect_settings(&cwd);
    let request_timeout = Duration::from_secs_f64(opts.wait_timeout_s.clamp(5.0, 30.0));
    let wait_timeout = timeout_from_secs(opts.wait_timeout_s)?;
    let poll_interval = timeout_from_secs(opts.poll_interval_s)?;
    let result = crawl_url(
        &opts,
        &settings,
        request_timeout,
        wait_timeout,
        poll_interval,
    )?;
    print_crawl_result(&result, opts.json, opts.full)
}

fn crawl_url(
    opts: &UrlCrawlOpts,
    settings: &UrlInspectSettings,
    request_timeout: Duration,
    wait_timeout: Duration,
    poll_interval: Duration,
) -> Result<UrlCrawlResult> {
    let (account_id, api_token) = cloudflare_credentials()?.ok_or_else(|| {
        anyhow::anyhow!(
            "Cloudflare crawl requires CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN in shell env or Flow personal env store"
        )
    })?;
    let mut result = crawl_via_cloudflare(
        opts,
        settings,
        request_timeout,
        wait_timeout,
        poll_interval,
        &account_id,
        &api_token,
        CLOUDFLARE_API_BASE,
    )?;
    if result.reference.is_empty() {
        result.reference = opts.url.clone();
    }
    Ok(result)
}

fn print_crawl_result(result: &UrlCrawlResult, json_output: bool, full: bool) -> Result<()> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(result)
                .context("failed to encode crawl result as json")?
        );
        return Ok(());
    }

    println!("Provider: {}", result.provider);
    println!("Job: {}", result.job_id);
    println!("Status: {}", result.status);
    println!("URL: {}", result.reference);
    if let Some(total) = result.total {
        println!("Total: {}", total);
    }
    if let Some(finished) = result.finished {
        println!("Finished: {}", finished);
    }
    if let Some(browser_seconds_used) = result.browser_seconds_used {
        println!("Browser seconds: {:.2}", browser_seconds_used);
    }
    if !result.records.is_empty() {
        println!("\nRecords:");
        for (index, record) in result.records.iter().enumerate() {
            let label = record.title.as_deref().unwrap_or(&record.url);
            println!("{}. {}", index + 1, label);
            println!("   URL: {}", record.url);
            println!("   Status: {}", record.status);
            if let Some(status_code) = record.status_code {
                println!("   HTTP: {}", status_code);
            }
            if let Some(excerpt) = record.excerpt.as_deref() {
                println!("   Excerpt: {}", excerpt);
            }
            if full && let Some(markdown) = record.markdown.as_deref() {
                println!("\n   Markdown:\n{}\n", markdown);
            }
        }
        if !full
            && result
                .records
                .iter()
                .any(|record| record.markdown.is_some())
        {
            println!("\nHint: pass --full to print markdown bodies for returned records.");
        }
    }
    Ok(())
}

fn inspect_url(
    opts: &UrlInspectOpts,
    settings: &UrlInspectSettings,
    timeout: Duration,
) -> Result<UrlInspectResult> {
    let provider = opts.provider;
    let (cloudflare_creds, cloudflare_error) = match cloudflare_credentials() {
        Ok(value) => (value, None),
        Err(err) => (None, Some(format!("{err:#}"))),
    };
    let scraper_ready = settings.scraper_base_url.is_some();
    let direct_allowed = settings.allow_direct_fallback || !scraper_ready;

    let plan: Vec<UrlInspectProvider> = match provider {
        UrlInspectProvider::Auto => {
            let mut providers = Vec::new();
            if cloudflare_creds.is_some() {
                providers.push(UrlInspectProvider::Cloudflare);
            }
            if scraper_ready {
                providers.push(UrlInspectProvider::Scraper);
            }
            providers.push(UrlInspectProvider::Direct);
            providers
        }
        UrlInspectProvider::Cloudflare => vec![UrlInspectProvider::Cloudflare],
        UrlInspectProvider::Scraper => {
            if settings.allow_direct_fallback {
                vec![UrlInspectProvider::Scraper, UrlInspectProvider::Direct]
            } else {
                vec![UrlInspectProvider::Scraper]
            }
        }
        UrlInspectProvider::Direct => vec![UrlInspectProvider::Direct],
    };

    let mut errors = Vec::new();
    for next in plan {
        match next {
            UrlInspectProvider::Cloudflare => {
                let Some((account_id, api_token)) = cloudflare_creds.clone() else {
                    if let Some(err) = cloudflare_error.as_deref() {
                        errors.push(format!("cloudflare: {err}"));
                    } else {
                        errors.push(
                            "cloudflare: missing CLOUDFLARE_ACCOUNT_ID or CLOUDFLARE_API_TOKEN"
                                .to_string(),
                        );
                    }
                    continue;
                };
                if account_id.trim().is_empty() || api_token.trim().is_empty() {
                    errors.push(
                        "cloudflare: missing CLOUDFLARE_ACCOUNT_ID or CLOUDFLARE_API_TOKEN"
                            .to_string(),
                    );
                    continue;
                }
                match inspect_via_cloudflare_markdown(
                    &opts.url,
                    timeout,
                    &account_id,
                    &api_token,
                    settings.cache_ttl_hours,
                    CLOUDFLARE_API_BASE,
                    opts.full,
                ) {
                    Ok(result) => return Ok(result),
                    Err(err) => errors.push(format!("cloudflare: {err:#}")),
                }
            }
            UrlInspectProvider::Scraper => {
                if let Some(base_url) = settings.scraper_base_url.as_deref() {
                    match inspect_via_scraper(
                        &opts.url,
                        timeout,
                        base_url,
                        settings.scraper_api_key.as_deref(),
                        opts.full,
                    ) {
                        Ok(result) => return Ok(result),
                        Err(err) => {
                            errors.push(format!("scraper: {err:#}"));
                            if !direct_allowed && provider == UrlInspectProvider::Scraper {
                                break;
                            }
                        }
                    }
                } else {
                    errors.push("scraper: no scraper_base_url configured".to_string());
                }
            }
            UrlInspectProvider::Direct => {
                match inspect_via_direct_fetch(&opts.url, timeout, opts.full) {
                    Ok(result) => return Ok(result),
                    Err(err) => errors.push(format!("direct: {err:#}")),
                }
            }
            UrlInspectProvider::Auto => {}
        }
    }

    if errors.is_empty() {
        bail!("url inspect failed with no available providers");
    }

    bail!("url inspect failed:\n- {}", errors.join("\n- "))
}

fn print_result(result: &UrlInspectResult, json_output: bool, full: bool) -> Result<()> {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(result).context("failed to encode result as json")?
        );
        return Ok(());
    }

    println!("Provider: {}", result.provider);
    if let Some(title) = &result.title {
        println!("Title: {title}");
    }
    if let Some(final_url) = &result.final_url {
        println!("URL: {final_url}");
    } else {
        println!("URL: {}", result.reference);
    }
    if let Some(content_type) = &result.content_type {
        println!("Content-Type: {content_type}");
    }
    if let Some(description) = &result.description {
        println!("\nDescription:\n{description}");
    }
    if let Some(excerpt) = &result.excerpt {
        println!("\nExcerpt:\n{excerpt}");
    }
    if full {
        if let Some(markdown) = &result.markdown {
            println!("\nMarkdown:\n{markdown}");
        }
    } else if result.markdown.is_some() {
        println!("\nHint: pass --full to print the full markdown body.");
    }
    Ok(())
}

fn render_compact_result(result: &UrlInspectResult) -> String {
    let mut lines = Vec::new();
    lines.push(format!("- URL: {}", result.reference));
    lines.push(format!("- Provider: {}", result.provider));
    if let Some(final_url) = result.final_url.as_deref()
        && final_url != result.reference
    {
        lines.push(format!("- Final URL: {final_url}"));
    }
    if let Some(title) = result.title.as_deref() {
        lines.push(format!("- Title: {title}"));
    }
    if let Some(description) = result.description.as_deref() {
        lines.push(format!("- Description: {description}"));
    }
    if let Some(excerpt) = result.excerpt.as_deref() {
        lines.push(format!("- Excerpt: {excerpt}"));
    }
    if let Some(content_type) = result.content_type.as_deref() {
        lines.push(format!("- Content-Type: {content_type}"));
    }
    lines.join("\n")
}

fn crawl_via_cloudflare(
    opts: &UrlCrawlOpts,
    settings: &UrlInspectSettings,
    request_timeout: Duration,
    wait_timeout: Duration,
    poll_interval: Duration,
    account_id: &str,
    api_token: &str,
    api_base: &str,
) -> Result<UrlCrawlResult> {
    let client = http_client::blocking_with_timeout(request_timeout)?;
    let endpoint = format!(
        "{}/accounts/{}/browser-rendering/crawl",
        api_base.trim_end_matches('/'),
        account_id
    );
    let create_payload = cloudflare_crawl_create_payload(opts, settings);
    let response = client
        .post(&endpoint)
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, format!("Bearer {api_token}"))
        .json(&create_payload)
        .send()
        .context("failed to create Cloudflare Browser Rendering crawl job")?;
    let status = response.status();
    let body = response
        .text()
        .context("failed to read Cloudflare crawl create response")?;
    if !status.is_success() {
        bail!("http {}: {}", status.as_u16(), body);
    }

    let envelope: CloudflareEnvelope =
        serde_json::from_str(&body).context("failed to decode Cloudflare crawl create response")?;
    if !envelope.success {
        bail!(
            "Cloudflare crawl failed: {}",
            cloudflare_error_detail(&envelope.errors)
        );
    }
    let job_id = envelope
        .result
        .as_ref()
        .and_then(|value| value.as_str().map(|v| v.to_string()))
        .or_else(|| {
            envelope.result.as_ref().and_then(|value| {
                value
                    .as_object()
                    .and_then(|obj| obj.get("id"))
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string())
            })
        })
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Cloudflare crawl did not return a job id"))?;

    let started = Instant::now();
    loop {
        let state =
            fetch_cloudflare_crawl_result(&client, &endpoint, api_token, &job_id, 1, false, false)?;
        match state.status.as_str() {
            "completed" => {
                return fetch_cloudflare_crawl_result(
                    &client,
                    &endpoint,
                    api_token,
                    &job_id,
                    opts.records.max(1),
                    true,
                    opts.full,
                );
            }
            "failed" | "cancelled" | "canceled" => {
                bail!(
                    "Cloudflare crawl job {} ended with status {}",
                    job_id,
                    state.status
                );
            }
            _ => {
                if started.elapsed() >= wait_timeout {
                    bail!(
                        "timed out waiting for Cloudflare crawl job {} after {:.1}s",
                        job_id,
                        wait_timeout.as_secs_f64()
                    );
                }
                std::thread::sleep(poll_interval);
            }
        }
    }
}

fn cloudflare_crawl_create_payload(
    opts: &UrlCrawlOpts,
    settings: &UrlInspectSettings,
) -> serde_json::Value {
    let max_age_s = opts.max_age_s.or_else(|| {
        settings
            .cache_ttl_hours
            .map(|hours| (hours * 3600.0).round().clamp(0.0, 86_400.0) as u64)
    });
    let mut payload = json!({
        "url": opts.url,
        "limit": opts.limit,
        "depth": opts.depth,
        "render": opts.render,
        "source": cloudflare_crawl_source(opts.source),
        "formats": ["markdown"],
    });

    if let Some(max_age_s) = max_age_s {
        payload["maxAge"] = json!(max_age_s);
    }

    let mut options = serde_json::Map::new();
    if opts.include_external_links {
        options.insert("includeExternalLinks".to_string(), json!(true));
    }
    if opts.include_subdomains {
        options.insert("includeSubdomains".to_string(), json!(true));
    }
    if !opts.include_patterns.is_empty() {
        options.insert("includePatterns".to_string(), json!(opts.include_patterns));
    }
    if !opts.exclude_patterns.is_empty() {
        options.insert("excludePatterns".to_string(), json!(opts.exclude_patterns));
    }
    if !options.is_empty() {
        payload["options"] = Value::Object(options);
    }

    payload
}

fn cloudflare_crawl_source(source: UrlCrawlSource) -> &'static str {
    match source {
        UrlCrawlSource::All => "all",
        UrlCrawlSource::Sitemaps => "sitemaps",
        UrlCrawlSource::Links => "links",
    }
}

fn fetch_cloudflare_crawl_result(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    api_token: &str,
    job_id: &str,
    records_limit: usize,
    completed_only: bool,
    include_full_markdown: bool,
) -> Result<UrlCrawlResult> {
    let mut request = client
        .get(format!("{}/{}", endpoint.trim_end_matches('/'), job_id))
        .header(AUTHORIZATION, format!("Bearer {api_token}"))
        .query(&[("limit", records_limit.max(1).to_string())]);
    if completed_only {
        request = request.query(&[("status", "completed")]);
    }
    let response = request
        .send()
        .context("failed to fetch Cloudflare crawl status")?;
    let status = response.status();
    let body = response
        .text()
        .context("failed to read Cloudflare crawl status response")?;
    if !status.is_success() {
        bail!("http {}: {}", status.as_u16(), body);
    }

    let envelope: CloudflareEnvelope =
        serde_json::from_str(&body).context("failed to decode Cloudflare crawl status response")?;
    if !envelope.success {
        bail!(
            "Cloudflare crawl status failed: {}",
            cloudflare_error_detail(&envelope.errors)
        );
    }

    let Some(result) = envelope.result.as_ref() else {
        bail!("Cloudflare crawl status returned no result");
    };
    parse_cloudflare_crawl_result(result, include_full_markdown)
}

fn parse_cloudflare_crawl_result(
    value: &Value,
    include_full_markdown: bool,
) -> Result<UrlCrawlResult> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Cloudflare crawl result was not an object"))?;
    let job_id = string_field(object, "id")
        .ok_or_else(|| anyhow::anyhow!("Cloudflare crawl result missing id"))?;
    let status = string_field(object, "status").unwrap_or_else(|| "unknown".to_string());
    let total = u64_field(object, "total");
    let finished = u64_field(object, "finished");
    let browser_seconds_used = f64_field(object, "browserSecondsUsed");
    let cursor = string_field(object, "cursor");
    let render = bool_field(object, "render").unwrap_or(false);
    let source = string_field(object, "source").unwrap_or_else(|| "all".to_string());
    let reference = string_field(object, "url").unwrap_or_default();
    let records = object
        .get("records")
        .and_then(|records| records.as_array())
        .map(|records| {
            records
                .iter()
                .filter_map(|record| parse_cloudflare_crawl_record(record, include_full_markdown))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(UrlCrawlResult {
        reference,
        provider: "cloudflare-crawl".to_string(),
        job_id,
        status,
        total,
        finished,
        browser_seconds_used,
        render,
        source,
        cursor,
        records,
    })
}

fn parse_cloudflare_crawl_record(
    value: &Value,
    include_full_markdown: bool,
) -> Option<UrlCrawlRecord> {
    let object = value.as_object()?;
    let metadata = object.get("metadata").and_then(|value| value.as_object());
    let url = string_field(object, "url")
        .or_else(|| metadata.and_then(|value| string_field(value, "url")))?;
    let status = string_field(object, "status").unwrap_or_else(|| "unknown".to_string());
    let title = metadata.and_then(|value| string_field(value, "title"));
    let status_code = metadata
        .and_then(|value| u64_field(value, "status"))
        .and_then(|value| u16::try_from(value).ok());
    let markdown = object
        .get("markdown")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string());
    let excerpt = markdown
        .as_deref()
        .map(markdown_metadata)
        .and_then(|metadata| metadata.description.or(metadata.excerpt));

    Some(UrlCrawlRecord {
        url,
        status,
        title,
        status_code,
        excerpt,
        markdown: include_full_markdown.then_some(markdown).flatten(),
    })
}

fn cloudflare_error_detail(errors: &[CloudflareError]) -> String {
    let detail = errors
        .iter()
        .map(|err| match (&err.code, &err.message) {
            (Some(code), Some(message)) => format!("{code}: {message}"),
            (_, Some(message)) => message.clone(),
            _ => "unknown Cloudflare error".to_string(),
        })
        .collect::<Vec<_>>()
        .join("; ");
    if detail.is_empty() {
        "unknown Cloudflare error".to_string()
    } else {
        detail
    }
}

fn inspect_via_cloudflare_markdown(
    url: &str,
    timeout: Duration,
    account_id: &str,
    api_token: &str,
    cache_ttl_hours: Option<f64>,
    api_base: &str,
    include_full_markdown: bool,
) -> Result<UrlInspectResult> {
    let client = http_client::blocking_with_timeout(timeout)?;
    let endpoint = format!(
        "{}/accounts/{}/browser-rendering/markdown",
        api_base.trim_end_matches('/'),
        account_id
    );
    let mut request = client
        .post(endpoint)
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, format!("Bearer {api_token}"))
        .json(&json!({ "url": url }));

    if let Some(hours) = cache_ttl_hours {
        let ttl = (hours * 3600.0).round().clamp(0.0, 86_400.0) as u32;
        request = request.query(&[("cacheTTL", ttl.to_string())]);
    }

    let response = request
        .send()
        .context("failed to call Cloudflare Browser Rendering markdown endpoint")?;
    let status = response.status();
    let body = response
        .text()
        .context("failed to read Cloudflare markdown response")?;
    if !status.is_success() {
        bail!("http {}: {}", status.as_u16(), body);
    }

    let envelope: CloudflareEnvelope =
        serde_json::from_str(&body).context("failed to decode Cloudflare markdown response")?;
    if !envelope.success {
        bail!(
            "Cloudflare markdown failed: {}",
            cloudflare_error_detail(&envelope.errors)
        );
    }

    let markdown = envelope
        .result
        .as_ref()
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let metadata = markdown_metadata(&markdown);
    Ok(UrlInspectResult {
        reference: url.to_string(),
        provider: "cloudflare-markdown".to_string(),
        final_url: Some(url.to_string()),
        status_code: Some(status.as_u16()),
        content_type: Some("text/markdown".to_string()),
        title: metadata.title,
        description: metadata.description,
        excerpt: metadata.excerpt,
        markdown: include_full_markdown.then_some(markdown),
        cache_hit: None,
    })
}

fn inspect_via_scraper(
    url: &str,
    timeout: Duration,
    base_url: &str,
    api_key: Option<&str>,
    _include_full_markdown: bool,
) -> Result<UrlInspectResult> {
    let client = http_client::blocking_with_timeout(timeout)?;
    let endpoint = format!("{}/scrape", base_url.trim_end_matches('/'));
    let mut request = client.post(endpoint).json(&json!({
        "url": url,
        "mode": "balanced",
        "timeout_s": timeout.as_secs_f64(),
        "max_bytes": 400_000_u64
    }));
    let api_token = api_key
        .map(|value| value.to_string())
        .or_else(|| std::env::var("SEQ_SCRAPER_API_KEY").ok());
    if let Some(token) = api_token {
        request = request.bearer_auth(token);
    }

    let response = request
        .send()
        .context("failed to call configured scraper endpoint")?;
    let status = response.status();
    let body = response
        .text()
        .context("failed to read scraper response body")?;
    if !status.is_success() {
        bail!("http {}: {}", status.as_u16(), body);
    }
    let payload: ScrapeResult =
        serde_json::from_str(&body).context("failed to decode scraper response")?;
    if !payload.success {
        bail!(
            "{}",
            payload
                .error
                .unwrap_or_else(|| "scraper reported failure without an error".to_string())
        );
    }

    Ok(UrlInspectResult {
        reference: url.to_string(),
        provider: "scraper".to_string(),
        final_url: payload.final_url,
        status_code: payload.status_code,
        content_type: payload.content_type,
        title: payload.title,
        description: None,
        excerpt: payload
            .text_excerpt
            .map(|value| truncate_excerpt(&normalize_whitespace(&value))),
        markdown: None,
        cache_hit: payload.cache_hit,
    })
}

fn inspect_via_direct_fetch(
    url: &str,
    timeout: Duration,
    include_full_markdown: bool,
) -> Result<UrlInspectResult> {
    let client = http_client::blocking_with_timeout(timeout)?;
    let response = client
        .get(url)
        .header(ACCEPT, DEFAULT_DIRECT_ACCEPT)
        .send()
        .with_context(|| format!("failed to fetch {url}"))?;
    let status = response.status();
    let final_url = response.url().to_string();
    let content_type = header_value(response.headers().get(CONTENT_TYPE));
    let markdown_tokens = header_value(response.headers().get("x-markdown-tokens"));
    let body = response.text().context("failed to read response body")?;

    if !status.is_success() {
        bail!("http {}: {}", status.as_u16(), truncate_excerpt(&body));
    }

    let looks_like_markdown = content_type
        .as_deref()
        .map(|value| value.contains("markdown"))
        .unwrap_or(false)
        || markdown_tokens.is_some();

    let (title, description, excerpt, markdown) = if looks_like_markdown {
        let metadata = markdown_metadata(&body);
        (
            metadata.title,
            metadata.description,
            metadata.excerpt,
            include_full_markdown.then_some(body),
        )
    } else if content_type
        .as_deref()
        .map(|value| value.contains("html"))
        .unwrap_or(false)
        || body.contains("<html")
        || body.contains("<body")
    {
        let html = html_metadata(&body, &final_url);
        (
            html.title,
            html.description,
            html.excerpt,
            include_full_markdown.then_some(body),
        )
    } else {
        let excerpt = truncate_excerpt(&normalize_whitespace(&body));
        (
            None,
            None,
            Some(excerpt),
            include_full_markdown.then_some(body),
        )
    };

    Ok(UrlInspectResult {
        reference: url.to_string(),
        provider: "direct".to_string(),
        final_url: Some(final_url),
        status_code: Some(status.as_u16()),
        content_type,
        title,
        description,
        excerpt,
        markdown,
        cache_hit: None,
    })
}

fn timeout_from_secs(seconds: f64) -> Result<Duration> {
    if !seconds.is_finite() || seconds <= 0.0 {
        bail!("timeout must be a positive finite number");
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn cloudflare_credentials() -> Result<Option<(String, String)>> {
    let account_id = load_secret_env_var("CLOUDFLARE_ACCOUNT_ID")?;
    let api_token = load_secret_env_var("CLOUDFLARE_API_TOKEN")?;
    match (account_id, api_token) {
        (Some(account_id), Some(api_token)) => Ok(Some((account_id, api_token))),
        (None, None) => Ok(None),
        (Some(_), None) => {
            bail!("missing CLOUDFLARE_API_TOKEN; set it in shell env or Flow personal env store")
        }
        (None, Some(_)) => {
            bail!("missing CLOUDFLARE_ACCOUNT_ID; set it in shell env or Flow personal env store")
        }
    }
}

fn load_secret_env_var(key: &str) -> Result<Option<String>> {
    if let Ok(value) = std::env::var(key) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_string()));
        }
    }

    let primary = flow_env::get_personal_env_var(key)
        .with_context(|| format!("failed to load {key} from Flow personal env store"));
    match primary {
        Ok(Some(value)) => {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Ok(Some(trimmed.to_string()));
            }
        }
        Ok(None) => {}
        Err(err) if wants_local_env_backend() => return Err(err),
        Err(_) => {}
    }

    if !wants_local_env_backend() {
        let local_value = with_local_env_backend(|| flow_env::get_personal_env_var(key))
            .with_context(|| format!("failed to load {key} from local Flow personal env store"))?;
        if let Some(value) = local_value {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Ok(Some(trimmed.to_string()));
            }
        }
    }

    Ok(None)
}

fn wants_local_env_backend() -> bool {
    if let Some(backend) = crate::config::preferred_env_backend() {
        return backend == "local";
    }
    if let Ok(value) = std::env::var("FLOW_ENV_BACKEND") {
        return value.trim().eq_ignore_ascii_case("local");
    }
    std::env::var("FLOW_ENV_LOCAL")
        .ok()
        .map(|value| value.trim() == "1" || value.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn with_local_env_backend<T>(action: impl FnOnce() -> Result<T>) -> Result<T> {
    let previous = std::env::var("FLOW_ENV_BACKEND").ok();
    unsafe {
        std::env::set_var("FLOW_ENV_BACKEND", "local");
    }
    let result = action();
    unsafe {
        match previous {
            Some(value) => std::env::set_var("FLOW_ENV_BACKEND", value),
            None => std::env::remove_var("FLOW_ENV_BACKEND"),
        }
    }
    result
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
}

fn u64_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<u64> {
    object.get(key).and_then(|value| match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse::<u64>().ok(),
        _ => None,
    })
}

fn f64_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<f64> {
    object.get(key).and_then(|value| match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    })
}

fn bool_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<bool> {
    object.get(key).and_then(|value| match value {
        Value::Bool(value) => Some(*value),
        Value::String(text) => match text.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    })
}

fn load_url_inspect_settings(cwd: &Path) -> UrlInspectSettings {
    let mut settings = UrlInspectSettings::default();

    let global_path = config::default_config_path();
    if global_path.exists() {
        let cfg = config::load_or_default(&global_path);
        merge_seq_settings(&mut settings, cfg.skills.and_then(|skills| skills.seq));
    }

    if let Some(local_flow_toml) = project_snapshot::find_flow_toml_upwards(cwd) {
        let cfg = config::load_or_default(&local_flow_toml);
        merge_seq_settings(&mut settings, cfg.skills.and_then(|skills| skills.seq));
    }

    settings
}

fn merge_seq_settings(settings: &mut UrlInspectSettings, seq_cfg: Option<config::SkillsSeqConfig>) {
    let Some(seq_cfg) = seq_cfg else {
        return;
    };

    if let Some(value) = seq_cfg.scraper_base_url {
        settings.scraper_base_url = Some(value);
    }
    if let Some(value) = seq_cfg.scraper_api_key {
        settings.scraper_api_key = Some(value);
    }
    if let Some(value) = seq_cfg.cache_ttl_hours {
        settings.cache_ttl_hours = Some(value);
    }
    if let Some(value) = seq_cfg.allow_direct_fallback {
        settings.allow_direct_fallback = value;
    }
}

#[derive(Debug, Default)]
struct TextMetadata {
    title: Option<String>,
    description: Option<String>,
    excerpt: Option<String>,
}

fn markdown_metadata(markdown: &str) -> TextMetadata {
    let (frontmatter, content) = extract_markdown_frontmatter(markdown);
    let mut title = None;
    let mut description = None;
    let mut headings = Vec::new();
    let mut pre_heading_lines = Vec::new();
    let mut post_heading_lines = Vec::new();
    let mut heading_seen = false;

    if let Some(frontmatter) = frontmatter.as_deref() {
        title = capture_frontmatter_value(frontmatter, "title");
        description = capture_frontmatter_value(frontmatter, "description")
            .or_else(|| capture_frontmatter_value(frontmatter, "summary"));
    }

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if looks_like_markdown_boilerplate(line) {
            continue;
        }
        if line.starts_with('#') {
            headings.push(line.to_string());
            heading_seen = true;
            continue;
        }
        if line.starts_with("```") {
            continue;
        }
        if looks_like_markdown_metadata_line(line) {
            continue;
        }
        if heading_seen {
            post_heading_lines.push(line.to_string());
        } else {
            pre_heading_lines.push(line.to_string());
        }
    }

    let first_heading = headings.iter().find_map(|heading| {
        let text = heading.trim_start_matches('#').trim();
        (!text.is_empty()).then(|| text.to_string())
    });

    if title.is_none() {
        title = first_heading.clone();
    }

    let paragraphs = if !post_heading_lines.is_empty() {
        post_heading_lines
    } else {
        pre_heading_lines
    };

    if description.is_none()
        && let Some(first) = paragraphs.first()
    {
        description = Some(truncate_excerpt(first));
    }

    let excerpt_source = paragraphs.join(" ");
    let excerpt = (!excerpt_source.is_empty()).then(|| truncate_excerpt(&excerpt_source));

    TextMetadata {
        title,
        description,
        excerpt,
    }
}

fn looks_like_markdown_boilerplate(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower == "search"
        || lower.starts_with("[skip to content]")
        || lower.starts_with("search ")
        || lower.contains("subscribe to rss")
        || lower.contains("view rss feeds")
        || lower.contains("select theme")
        || lower.contains("docs directory")
        || lower.contains("new updates and improvements at cloudflare")
        || lower == "help"
        || lower.contains("back to all posts")
        || lower.starts_with("![")
        || lower.starts_with("[ ![](")
        || (trimmed.starts_with('[') && trimmed.matches("](").count() >= 2)
    {
        return true;
    }

    matches!(trimmed, "# Changelog" | "## Changelog")
}

fn looks_like_markdown_metadata_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("_edit:")
        || lower.starts_with("*edit:")
        || lower.starts_with("edit:")
        || is_date_only_line(trimmed)
    {
        return true;
    }

    is_single_markdown_link_line(trimmed)
}

fn is_date_only_line(line: &str) -> bool {
    let Some((month, rest)) = line.split_once(' ') else {
        return false;
    };
    if !matches!(
        month,
        "Jan"
            | "January"
            | "Feb"
            | "February"
            | "Mar"
            | "March"
            | "Apr"
            | "April"
            | "May"
            | "Jun"
            | "June"
            | "Jul"
            | "July"
            | "Aug"
            | "August"
            | "Sep"
            | "Sept"
            | "September"
            | "Oct"
            | "October"
            | "Nov"
            | "November"
            | "Dec"
            | "December"
    ) {
        return false;
    }

    let Some((day, year)) = rest.split_once(',') else {
        return false;
    };
    let day = day.trim();
    let year = year.trim();
    !day.is_empty()
        && day.chars().all(|ch| ch.is_ascii_digit())
        && year.len() == 4
        && year.chars().all(|ch| ch.is_ascii_digit())
}

fn is_single_markdown_link_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix('[') else {
        return false;
    };
    let Some((label, url)) = rest.split_once("](") else {
        return false;
    };
    let Some(url) = url.strip_suffix(')') else {
        return false;
    };
    !label.trim().is_empty()
        && !url.trim().is_empty()
        && !label.contains('[')
        && !url.contains('(')
        && !url.contains(')')
}

fn html_metadata(html: &str, final_url: &str) -> TextMetadata {
    let title = capture_first(r"(?is)<title[^>]*>(.*?)</title>", html)
        .map(|value| normalize_whitespace(&value))
        .filter(|value| !value.is_empty());

    let description = capture_meta_description(html)
        .map(|value| normalize_whitespace(&value))
        .filter(|value| !value.is_empty())
        .map(|value| truncate_excerpt(&value));

    let excerpt = {
        let without_scripts = replace_all(r"(?is)<(script|style)[^>]*>.*?</\1>", html, " ");
        let without_tags = replace_all(r"(?is)<[^>]+>", &without_scripts, " ");
        let normalized = normalize_whitespace(&without_tags);
        (!normalized.is_empty()).then(|| truncate_excerpt(&normalized))
    };

    let mut metadata = TextMetadata {
        title,
        description,
        excerpt,
    };

    if looks_like_js_app_shell(final_url, html, &metadata) {
        if metadata.description.is_none() {
            metadata.description = Some(
                "JavaScript-heavy app shell; direct fetch could not extract structured page content. Prefer Browser Rendering markdown, a configured scraper, or a domain-specific resolver."
                    .to_string(),
            );
        }
        metadata.excerpt = None;
        if final_url.contains("linear.app/") && metadata.title.as_deref() == Some("Linear") {
            metadata.title = Some("Linear (app shell)".to_string());
        }
    }

    metadata
}

fn capture_meta_description(html: &str) -> Option<String> {
    capture_first(
        r#"(?is)<meta[^>]+(?:name|property)\s*=\s*["'](?:description|og:description)["'][^>]+content\s*=\s*["'](.*?)["'][^>]*>"#,
        html,
    )
    .or_else(|| {
        capture_first(
            r#"(?is)<meta[^>]+content\s*=\s*["'](.*?)["'][^>]+(?:name|property)\s*=\s*["'](?:description|og:description)["'][^>]*>"#,
            html,
        )
    })
}

fn capture_first(pattern: &str, haystack: &str) -> Option<String> {
    let regex = Regex::new(pattern).ok()?;
    let captures = regex.captures(haystack)?;
    captures
        .get(1)
        .map(|capture| capture.as_str().trim().to_string())
}

fn replace_all(pattern: &str, haystack: &str, replacement: &str) -> String {
    Regex::new(pattern)
        .map(|regex| regex.replace_all(haystack, replacement).into_owned())
        .unwrap_or_else(|_| haystack.to_string())
}

fn normalize_whitespace(input: &str) -> String {
    input
        .split_whitespace()
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_excerpt(input: &str) -> String {
    let normalized = normalize_whitespace(input);
    if normalized.chars().count() <= DEFAULT_EXCERPT_CHARS {
        return normalized;
    }
    let truncated: String = normalized.chars().take(DEFAULT_EXCERPT_CHARS).collect();
    format!("{}...", truncated.trim_end())
}

fn extract_markdown_frontmatter(markdown: &str) -> (Option<String>, String) {
    let mut lines = markdown.lines();
    let Some(first) = lines.next() else {
        return (None, markdown.to_string());
    };
    if first.trim() != "---" {
        return (None, markdown.to_string());
    }

    let mut frontmatter = Vec::new();
    let mut remainder = Vec::new();
    let mut found_closing = false;
    for line in lines {
        if !found_closing && line.trim() == "---" {
            found_closing = true;
            continue;
        }
        if found_closing {
            remainder.push(line);
        } else {
            frontmatter.push(line);
        }
    }

    if !found_closing {
        return (None, markdown.to_string());
    }

    (Some(frontmatter.join("\n")), remainder.join("\n"))
}

fn capture_frontmatter_value(frontmatter: &str, key: &str) -> Option<String> {
    let pattern = format!(r"(?mi)^\s*{}\s*:\s*(.+?)\s*$", regex::escape(key));
    let value = capture_first(&pattern, frontmatter)?;
    let value = value.trim().trim_matches('"').trim_matches('\'');
    (!value.is_empty()).then(|| value.to_string())
}

fn looks_like_js_app_shell(final_url: &str, html: &str, metadata: &TextMetadata) -> bool {
    let title = metadata.title.as_deref().unwrap_or_default();
    let excerpt = metadata.excerpt.as_deref().unwrap_or_default();
    (final_url.contains("linear.app/") && title == "Linear")
        || metadata.description.is_none()
            && (excerpt.contains("performance.mark(\"appStart\")")
                || excerpt.contains("--bg-sidebar-light")
                || excerpt.contains("--bg-base-color-dark")
                || html.contains("performance.mark(\"appStart\")")
                || html.contains("--bg-sidebar-light"))
}

fn header_value(value: Option<&reqwest::header::HeaderValue>) -> Option<String> {
    value
        .and_then(|header| header.to_str().ok())
        .map(|value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    #[test]
    fn markdown_metadata_prefers_first_heading_and_excerpt() {
        let metadata = markdown_metadata(
            "# Example Title\n\nFirst paragraph with useful context.\n\nSecond paragraph.",
        );
        assert_eq!(metadata.title.as_deref(), Some("Example Title"));
        assert_eq!(
            metadata.description.as_deref(),
            Some("First paragraph with useful context.")
        );
        assert!(
            metadata
                .excerpt
                .as_deref()
                .unwrap_or_default()
                .contains("First paragraph with useful context.")
        );
    }

    #[test]
    fn markdown_metadata_reads_frontmatter_title_and_description() {
        let metadata = markdown_metadata(
            "---\n\
title: Crawl entire websites with a single API call using Browser Rendering\n\
description: Browser Rendering's new /crawl endpoint crawls and renders a site.\n\
---\n\n# Ignored Heading\n\nBody paragraph.",
        );
        assert_eq!(
            metadata.title.as_deref(),
            Some("Crawl entire websites with a single API call using Browser Rendering")
        );
        assert_eq!(
            metadata.description.as_deref(),
            Some("Browser Rendering's new /crawl endpoint crawls and renders a site.")
        );
        assert!(
            metadata
                .excerpt
                .as_deref()
                .unwrap_or_default()
                .contains("Body paragraph.")
        );
    }

    #[test]
    fn direct_fetch_extracts_html_metadata() {
        let mut server = Server::new();
        let _mock = server
            .mock("GET", "/page")
            .with_status(200)
            .with_header("content-type", "text/html; charset=utf-8")
            .with_body(
                r#"
                <html>
                  <head>
                    <title>Flow URL Inspect</title>
                    <meta name="description" content="Thin summaries for AI sessions." />
                  </head>
                  <body>
                    <article>Useful body text for the excerpt.</article>
                  </body>
                </html>
                "#,
            )
            .create();

        let result = inspect_via_direct_fetch(
            &format!("{}/page", server.url()),
            Duration::from_secs(5),
            false,
        )
        .expect("direct fetch should succeed");
        assert_eq!(result.title.as_deref(), Some("Flow URL Inspect"));
        assert_eq!(
            result.description.as_deref(),
            Some("Thin summaries for AI sessions.")
        );
        assert!(
            result
                .excerpt
                .as_deref()
                .unwrap_or_default()
                .contains("Useful body text")
        );
    }

    #[test]
    fn html_metadata_detects_linear_app_shell() {
        let metadata = html_metadata(
            r#"
            <html>
              <head><title>Linear</title></head>
              <body>
                <script>performance.mark("appStart")</script>
                <style>:root{--bg-sidebar-light:#f5f5f5;--bg-base-color-dark:#0f0f11;}</style>
              </body>
            </html>
            "#,
            "https://linear.app/example-workspace/project/example/overview",
        );
        assert_eq!(metadata.title.as_deref(), Some("Linear (app shell)"));
        assert!(
            metadata
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("JavaScript-heavy app shell")
        );
        assert!(metadata.excerpt.is_none());
    }

    #[test]
    fn cloudflare_markdown_normalizes_result() {
        let mut server = Server::new();
        let _mock = server
            .mock("POST", "/accounts/test-account/browser-rendering/markdown")
            .match_query(mockito::Matcher::UrlEncoded("cacheTTL".into(), "7200".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                "{\n  \"success\": true,\n  \"result\": \"# Cloudflare Page\\n\\nRendered into markdown.\"\n}",
            )
            .create();

        let result = inspect_via_cloudflare_markdown(
            "https://example.com/docs",
            Duration::from_secs(5),
            "test-account",
            "secret-token",
            Some(2.0),
            &server.url(),
            false,
        )
        .expect("cloudflare markdown should succeed");

        assert_eq!(result.provider, "cloudflare-markdown");
        assert_eq!(result.title.as_deref(), Some("Cloudflare Page"));
        assert_eq!(
            result.description.as_deref(),
            Some("Rendered into markdown.")
        );
    }

    #[test]
    fn markdown_metadata_skips_changelog_boilerplate() {
        let metadata = markdown_metadata(concat!(
            "[Skip to content](#_top)\n",
            "Search\n",
            "[Docs Directory](https://example.com/directory)[APIs](https://example.com/api)Help\n",
            "# Changelog\n",
            "New updates and improvements at Cloudflare.\n",
            "[ Subscribe to RSS ](https://example.com/rss)\n",
            "![hero image](https://example.com/hero.svg)\n",
            "[ ← Back to all posts ](https://example.com)\n",
            "## Crawl entire websites with a single API call using Browser Rendering\n\n",
            "Mar 10, 2026\n",
            "[ Browser Rendering ](https://example.com/browser-rendering)\n",
            "_Edit: this post has been edited to clarify crawling behavior._\n\n",
            "Browser Rendering's new /crawl endpoint lets you submit a starting URL and automatically discover content.\n",
        ));

        assert_eq!(
            metadata.title.as_deref(),
            Some("Crawl entire websites with a single API call using Browser Rendering")
        );
        assert_eq!(
            metadata.description.as_deref(),
            Some(
                "Browser Rendering's new /crawl endpoint lets you submit a starting URL and automatically discover content."
            )
        );
    }

    #[test]
    fn parse_cloudflare_crawl_result_extracts_records() {
        let payload = json!({
            "id": "crawl-job-123",
            "status": "completed",
            "url": "https://developers.cloudflare.com/browser-rendering/",
            "total": 3,
            "finished": 3,
            "browserSecondsUsed": 0.72,
            "render": false,
            "source": "all",
            "records": [
                {
                    "url": "https://developers.cloudflare.com/browser-rendering/rest-api/crawl-endpoint/",
                    "status": "completed",
                    "markdown": "# Crawl endpoint\n\nCloudflare can crawl and return markdown.",
                    "metadata": {
                        "title": "Crawl endpoint",
                        "status": 200
                    }
                }
            ]
        });

        let result =
            parse_cloudflare_crawl_result(&payload, false).expect("crawl result should parse");
        assert_eq!(result.provider, "cloudflare-crawl");
        assert_eq!(result.job_id, "crawl-job-123");
        assert_eq!(result.status, "completed");
        assert_eq!(result.total, Some(3));
        assert_eq!(result.finished, Some(3));
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].title.as_deref(), Some("Crawl endpoint"));
        assert_eq!(result.records[0].status_code, Some(200));
        assert_eq!(
            result.records[0].excerpt.as_deref(),
            Some("Cloudflare can crawl and return markdown.")
        );
        assert!(result.records[0].markdown.is_none());
    }
}
