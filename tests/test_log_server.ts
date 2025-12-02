#!/usr/bin/env bun
/**
 * Test script for flow log server ingestion and query.
 * Run: bun tests/test_log_server.ts
 */

const SERVER_URL = "http://127.0.0.1:9060";

interface LogEntry {
  project: string;
  content: string;
  timestamp: number;
  type: string;
  service: string;
  stack?: string;
  format: string;
}

interface StoredLogEntry {
  id: number;
  project: string;
  content: string;
  timestamp: number;
  type: string;
  service: string;
  stack?: string;
  format: string;
}

async function checkHealth(): Promise<boolean> {
  try {
    const res = await fetch(`${SERVER_URL}/health`);
    const data = await res.json();
    console.log("✓ Health check:", data);
    return data.status === "ok";
  } catch (e) {
    console.error("✗ Health check failed:", e);
    return false;
  }
}

async function ingestLog(entry: LogEntry): Promise<{ inserted: number; ids: number[] } | null> {
  try {
    console.log("\n→ Ingesting log:", JSON.stringify(entry));
    const res = await fetch(`${SERVER_URL}/logs/ingest`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(entry),
    });

    console.log("  Response status:", res.status);
    const text = await res.text();
    console.log("  Response body:", text);

    if (!res.ok) {
      console.error("✗ Ingest failed with status:", res.status);
      return null;
    }

    return JSON.parse(text);
  } catch (e) {
    console.error("✗ Ingest error:", e);
    return null;
  }
}

async function queryLogs(project?: string): Promise<StoredLogEntry[]> {
  try {
    const url = project
      ? `${SERVER_URL}/logs/query?project=${encodeURIComponent(project)}`
      : `${SERVER_URL}/logs/query`;

    console.log("\n→ Querying logs:", url);
    const res = await fetch(url);

    console.log("  Response status:", res.status);
    const text = await res.text();
    console.log("  Response body:", text);

    if (!res.ok) {
      console.error("✗ Query failed with status:", res.status);
      return [];
    }

    return JSON.parse(text);
  } catch (e) {
    console.error("✗ Query error:", e);
    return [];
  }
}

async function main() {
  console.log("=== Flow Log Server Test ===\n");
  console.log("Server URL:", SERVER_URL);

  // 1. Health check
  console.log("\n--- Step 1: Health Check ---");
  const healthy = await checkHealth();
  if (!healthy) {
    console.error("\n✗ Server is not healthy. Make sure 'f server' is running.");
    process.exit(1);
  }

  // 2. Query existing logs (baseline)
  console.log("\n--- Step 2: Query Existing Logs (baseline) ---");
  const existingLogs = await queryLogs();
  console.log(`Found ${existingLogs.length} existing logs`);

  // 3. Ingest a test log
  console.log("\n--- Step 3: Ingest Test Log ---");
  const testEntry: LogEntry = {
    project: "test-project",
    content: `Test log at ${new Date().toISOString()}`,
    timestamp: Date.now(),
    type: "log",
    service: "test-runner",
    format: "text",
  };

  const ingestResult = await ingestLog(testEntry);
  if (!ingestResult) {
    console.error("\n✗ Failed to ingest log");
    process.exit(1);
  }
  console.log("✓ Ingested:", ingestResult);

  // 4. Ingest an error log with stack trace
  console.log("\n--- Step 4: Ingest Error Log ---");
  const errorEntry: LogEntry = {
    project: "test-project",
    content: "TypeError: Cannot read property 'foo' of undefined",
    timestamp: Date.now(),
    type: "error",
    service: "api",
    stack: "at Object.<anonymous> (test.ts:10:5)\nat Module._compile (node:internal/modules/cjs/loader:1234:14)",
    format: "text",
  };

  const errorResult = await ingestLog(errorEntry);
  if (!errorResult) {
    console.error("\n✗ Failed to ingest error log");
    process.exit(1);
  }
  console.log("✓ Ingested error:", errorResult);

  // 5. Query logs for our test project
  console.log("\n--- Step 5: Query Test Project Logs ---");
  const projectLogs = await queryLogs("test-project");
  console.log(`Found ${projectLogs.length} logs for test-project`);

  // 6. Query all logs
  console.log("\n--- Step 6: Query All Logs ---");
  const allLogs = await queryLogs();
  console.log(`Found ${allLogs.length} total logs`);

  // 7. Verify results
  console.log("\n--- Step 7: Verification ---");
  if (projectLogs.length >= 2) {
    console.log("✓ Successfully ingested and queried logs!");
    console.log("\nSample log entry:");
    console.log(JSON.stringify(projectLogs[0], null, 2));
  } else {
    console.error("✗ Expected at least 2 logs, got:", projectLogs.length);
    console.error("This suggests logs are not being persisted correctly.");
    process.exit(1);
  }

  console.log("\n=== All tests passed! ===");
}

main().catch(console.error);
