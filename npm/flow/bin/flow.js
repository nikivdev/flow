#!/usr/bin/env node
const { spawn } = require("node:child_process");
const { existsSync } = require("node:fs");
const path = require("node:path");

const BIN_NAME = "f";

function resolveTarget() {
  const { platform, arch } = process;
  if (platform === "darwin" && arch === "arm64") return "aarch64-apple-darwin";
  if (platform === "darwin" && arch === "x64") return "x86_64-apple-darwin";
  if (platform === "linux" && arch === "arm64") return "aarch64-unknown-linux-gnu";
  if (platform === "linux" && arch === "x64") return "x86_64-unknown-linux-gnu";
  return null;
}

function getUpdatedPath(newDirs) {
  const pathSep = process.platform === "win32" ? ";" : ":";
  const existingPath = process.env.PATH || "";
  return [...newDirs, ...existingPath.split(pathSep).filter(Boolean)].join(pathSep);
}

function detectPackageManager() {
  const userAgent = process.env.npm_config_user_agent || "";
  if (/\bbun\//.test(userAgent)) return "bun";

  const execPath = process.env.npm_execpath || "";
  if (execPath.includes("bun")) return "bun";

  if (
    __dirname.includes(".bun/install/global") ||
    __dirname.includes(".bun\\install\\global")
  ) {
    return "bun";
  }

  return userAgent ? "npm" : null;
}

const target = resolveTarget();
if (!target) {
  console.error(`Unsupported platform: ${process.platform} (${process.arch})`);
  process.exit(1);
}

const vendorRoot = path.join(__dirname, "..", "vendor");
const archRoot = path.join(vendorRoot, target);
const binDir = path.join(archRoot, "flow");
const binName = process.platform === "win32" ? `${BIN_NAME}.exe` : BIN_NAME;
const binaryPath = path.join(binDir, binName);

if (!existsSync(binaryPath)) {
  console.error(`Missing binary: ${binaryPath}`);
  console.error("Try reinstalling the package or rebuilding the npm vendor artifacts.");
  process.exit(1);
}

const extraDirs = [];
const pathDir = path.join(archRoot, "path");
if (existsSync(pathDir)) {
  extraDirs.push(pathDir);
}
extraDirs.push(binDir);

const env = { ...process.env, PATH: getUpdatedPath(extraDirs) };
const manager = detectPackageManager();
if (manager === "bun") {
  env.FLOW_MANAGED_BY_BUN = "1";
} else if (manager) {
  env.FLOW_MANAGED_BY_NPM = "1";
}

const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
  env,
});

child.on("error", (err) => {
  console.error(err);
  process.exit(1);
});

const forwardSignal = (signal) => {
  if (child.killed) return;
  try {
    child.kill(signal);
  } catch {}
};

["SIGINT", "SIGTERM", "SIGHUP"].forEach((sig) => {
  process.on(sig, () => forwardSignal(sig));
});

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
  } else {
    process.exit(code ?? 1);
  }
});
