import fs from "node:fs/promises"
import path from "node:path"
import { execFileSync } from "node:child_process"
import { deflateRawSync } from "node:zlib"

const args = process.argv.slice(2)
let configPath = ".ai/health-checks.json"
for (let i = 0; i < args.length; i += 1) {
  if (args[i] === "--config" && args[i + 1]) {
    configPath = args[i + 1]
    i += 1
  }
}

const resolvedPath = path.resolve(configPath)
let config
try {
  const raw = await fs.readFile(resolvedPath, "utf8")
  config = JSON.parse(raw)
} catch (err) {
  if (err && err.code === "ENOENT") {
    console.log(`No health checks configured at ${configPath}; skipping.`)
    process.exit(0)
  }
  throw err
}

const checks = Array.isArray(config.checks) ? config.checks : []
if (checks.length === 0) {
  console.log(`No health checks configured in ${configPath}; skipping.`)
  process.exit(0)
}

const defaultBaseUrl = resolveDefaultBaseUrl(config)
const defaultTimeoutMs =
  typeof config.timeout_ms === "number" ? config.timeout_ms : 10000

console.log(`Running ${checks.length} health check(s) from ${configPath}`)

let failures = 0

for (const check of checks) {
  try {
    if (check.type === "gitedit-share") {
      const results = await runGiteditShareCheck(check, {
        baseUrl: resolveBaseUrl(check, defaultBaseUrl),
        timeoutMs: resolveTimeoutMs(check, defaultTimeoutMs),
      })
      for (const result of results) {
        if (result.ok) {
          console.log(`OK: ${result.name}`)
        } else {
          failures += 1
          console.log(`FAIL: ${result.name} - ${result.message}`)
        }
      }
      continue
    }

    if (check.type === "http") {
      const result = await runHttpCheck(check, {
        baseUrl: resolveBaseUrl(check, defaultBaseUrl, false),
        timeoutMs: resolveTimeoutMs(check, defaultTimeoutMs),
      })
      if (result.ok) {
        console.log(`OK: ${result.name}`)
      } else {
        failures += 1
        console.log(`FAIL: ${result.name} - ${result.message}`)
      }
      continue
    }

    failures += 1
    console.log(`FAIL: ${check.name || "unnamed"} - unknown type`)
  } catch (err) {
    failures += 1
    const name = check.name || check.type || "health check"
    console.log(
      `FAIL: ${name} - ${err instanceof Error ? err.message : String(err)}`,
    )
  }
}

if (failures > 0) {
  console.log(`${failures} health check(s) failed.`)
  process.exit(1)
} else {
  console.log("All health checks passed.")
}

function resolveDefaultBaseUrl(configValue) {
  const raw =
    configValue.baseUrl ?? configValue.base_url ?? process.env.HEALTH_BASE_URL
  if (!raw) return ""
  return expandEnv(String(raw))
}

function resolveBaseUrl(check, fallback, required = true) {
  const raw = check.baseUrl ?? check.base_url ?? fallback
  if (!raw && required) {
    throw new Error("Missing baseUrl for health check.")
  }
  if (!raw) return ""
  return expandEnv(String(raw))
}

function resolveTimeoutMs(check, fallback) {
  return typeof check.timeout_ms === "number" ? check.timeout_ms : fallback
}

function expandEnv(value) {
  if (typeof value !== "string") return value
  return value.replace(/\$\{([A-Z0-9_]+)\}/gi, (match, name) => {
    const envValue = process.env[name]
    if (envValue === undefined) {
      throw new Error(`Missing environment variable: ${name}`)
    }
    return envValue
  })
}

async function runHttpCheck(check, { baseUrl, timeoutMs }) {
  const name = check.name || check.url || "http check"
  const rawUrl = expandEnv(check.url ?? "")
  if (!rawUrl) {
    return { ok: false, name, message: "Missing url" }
  }
  const resolvedUrl = resolveUrl(rawUrl, baseUrl)
  const expectedStatuses = normalizeStatuses(check.expect_status ?? 200)
  const contains = normalizeContains(check.contains)
  const method = check.method ? String(check.method).toUpperCase() : "GET"
  const headers = check.headers && typeof check.headers === "object"
    ? check.headers
    : undefined

  const response = await fetchWithTimeout(resolvedUrl, { method, headers }, timeoutMs)
  if (!expectedStatuses.includes(response.status)) {
    return {
      ok: false,
      name,
      message: `Expected ${expectedStatuses.join(",")}, got ${response.status}`,
    }
  }

  if (contains.length > 0) {
    const body = await response.text()
    for (const needle of contains) {
      if (!body.includes(needle)) {
        return { ok: false, name, message: `Missing text: ${needle}` }
      }
    }
  }

  return { ok: true, name }
}

async function runGiteditShareCheck(check, { baseUrl, timeoutMs }) {
  const name = check.name || "gitedit share"
  const owner = check.owner || ""
  const repo = check.repo || ""
  const commitRef = check.commit || "HEAD"
  if (!owner || !repo) {
    return [
      {
        ok: false,
        name,
        message: "Missing owner or repo",
      },
    ]
  }

  const commitSha = resolveCommitSha(commitRef)
  const payload = buildSharePayload({ owner, repo, commitSha })
  const hash = encodeSharePayload(payload)
  const apiUrl = new URL(`/api/mirrors/share/${hash}`, baseUrl).toString()
  const pageUrl = new URL(`/${hash}`, baseUrl).toString()

  const results = []
  if (check.check_api !== false) {
    const response = await fetchWithTimeout(apiUrl, {}, timeoutMs)
    if (!response.ok) {
      results.push({
        ok: false,
        name: `${name} api`,
        message: `HTTP ${response.status}`,
      })
    } else {
      const data = await response.json().catch(() => null)
      if (!data || data.commit?.commit_sha !== commitSha) {
        results.push({
          ok: false,
          name: `${name} api`,
          message: "Unexpected response payload",
        })
      } else {
        results.push({ ok: true, name: `${name} api` })
      }
    }
  }

  if (check.check_page !== false) {
    const response = await fetchWithTimeout(pageUrl, {}, timeoutMs)
    if (!response.ok) {
      results.push({
        ok: false,
        name: `${name} page`,
        message: `HTTP ${response.status}`,
      })
    } else {
      results.push({ ok: true, name: `${name} page` })
    }
  }

  return results
}

function resolveCommitSha(ref) {
  try {
    return execFileSync("git", ["rev-parse", ref], {
      encoding: "utf8",
    }).trim()
  } catch (err) {
    if (/^[0-9a-f]{7,40}$/i.test(ref)) return ref
    throw err
  }
}

function buildSharePayload({ owner, repo, commitSha }) {
  return {
    v: 1,
    owner,
    repo,
    commit: {
      commit_sha: commitSha,
      commit_message: null,
      author_name: null,
      author_email: null,
      branch: null,
      ref: null,
      event: "commit",
      source: "flow-cli",
      session_hash: null,
      ai_sessions: [],
      received_at: new Date().toISOString(),
    },
  }
}

function encodeSharePayload(payload) {
  const json = JSON.stringify(payload)
  const compressed = deflateRawSync(Buffer.from(json))
  const b64 = compressed.toString("base64")
  return b64.replace(/\+/g, "-").replace(/\//g, "_")
}

function normalizeStatuses(value) {
  if (Array.isArray(value)) {
    return value.map((item) => Number(item)).filter((item) => !Number.isNaN(item))
  }
  const numberValue = Number(value)
  return Number.isNaN(numberValue) ? [200] : [numberValue]
}

function normalizeContains(value) {
  if (!value) return []
  if (Array.isArray(value)) return value.map(String)
  return [String(value)]
}

function resolveUrl(url, baseUrl) {
  if (/^https?:\/\//i.test(url)) return url
  if (!baseUrl) {
    throw new Error(`Relative url requires baseUrl: ${url}`)
  }
  return new URL(url, baseUrl).toString()
}

async function fetchWithTimeout(url, options, timeoutMs) {
  const controller = new AbortController()
  const timeout = setTimeout(() => controller.abort(), timeoutMs)
  try {
    return await fetch(url, { ...options, signal: controller.signal })
  } finally {
    clearTimeout(timeout)
  }
}
