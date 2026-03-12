# `f url`

Inspect or crawl URLs into compact AI-friendly summaries.

## Quick Start

```bash
# Thin single-page summary
f url inspect https://developers.cloudflare.com/changelog/post/2026-03-10-br-crawl-endpoint/

# Force Cloudflare Browser Rendering markdown
f url inspect --provider cloudflare https://developers.cloudflare.com/changelog/post/2026-03-10-br-crawl-endpoint/

# Machine-readable output
f url inspect --json https://linear.app/example-workspace/project/example-project-v1-1234567890ab/overview

# Explicit site crawl (Cloudflare Browser Rendering)
f url crawl https://developers.cloudflare.com/browser-rendering/rest-api/crawl-endpoint/ --limit 3 --records 2
```

## `inspect`

`f url inspect <url>` uses this provider order:

1. Cloudflare Browser Rendering markdown
2. configured scraper backend from `[skills.seq]`
3. direct fetch fallback

Default output is intentionally compact:

- title
- URL
- content type
- short description
- short excerpt

Use `--full` to include the full markdown/content body.

## `crawl`

`f url crawl <url>` is the explicit multi-page path.

It currently uses Cloudflare Browser Rendering crawl and polls until the job completes or the wait timeout is reached.

Useful flags:

```bash
f url crawl <url> --limit 10 --records 5
f url crawl <url> --depth 2 --render
f url crawl <url> --include-pattern "https://developers.cloudflare.com/browser-rendering/*"
f url crawl <url> --exclude-pattern "*/changelog/*"
f url crawl <url> --json
```

Defaults are tuned to stay small:

- `--limit 10`
- `--depth 2`
- `--records 5`
- `--render false`

## Auth

Cloudflare auth is read from:

1. shell env
2. Flow personal env store fallback

Required keys:

- `CLOUDFLARE_ACCOUNT_ID`
- `CLOUDFLARE_API_TOKEN`

No daemon is required.

## Config

If you have a local scraper backend, `f url inspect` reuses `[skills.seq]` settings from repo `flow.toml` or global `~/.config/flow/flow.toml`:

```toml
[skills.seq]
scraper_base_url = "http://127.0.0.1:7444"
scraper_api_key = "..."
cache_ttl_hours = 2
allow_direct_fallback = true
```
