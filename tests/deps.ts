#!/usr/bin/env tsx
/**
 * Quick e2e check that tasks with managed deps run inside the generated env.
 * Uses a fake `flox` shim so no real installs or network calls are needed.
 */

import { mkdtempSync, writeFileSync, chmodSync, mkdirSync } from "fs";
import { tmpdir } from "os";
import { join } from "path";
import { spawnSync } from "child_process";

function assertOk(result: ReturnType<typeof spawnSync>, context: string) {
  if (result.status !== 0) {
    const stdout = result.stdout?.toString() ?? "";
    const stderr = result.stderr?.toString() ?? "";
    throw new Error(
      `${context} failed (code ${result.status})\nstdout:\n${stdout}\nstderr:\n${stderr}`
    );
  }
}

function main() {
  const base = mkdtempSync(join(tmpdir(), "flow-flox-test-"));
  const binDir = join(base, "bin");
  mkdirSync(binDir, { recursive: true });

  const fakeFlox = join(base, "flox");
  const helloDep = join(binDir, "hello-dep");

  // Fake flox binary: lock-manifest echoes the manifest; activate adds our bin to PATH then execs the command.
  const floxScript = `#!/usr/bin/env bash
set -e
if [[ "$1" == "lock-manifest" ]]; then
  cat "$2"
  exit 0
fi
if [[ "$1" == "activate" ]]; then
  shift
  while [[ "$1" != "--" && "$#" -gt 0 ]]; do shift; done
  shift || true
  export PATH="${binDir}:$PATH"
  exec "$@"
fi
printf "unknown flox args: %s\n" "$@" 1>&2
exit 1
`;
  writeFileSync(fakeFlox, floxScript, { encoding: "utf8" });
  chmodSync(fakeFlox, 0o755);

  // Fake dependency command
  writeFileSync(helloDep, "#!/usr/bin/env bash\necho from-managed-env\n", {
    encoding: "utf8",
  });
  chmodSync(helloDep, 0o755);

  // flow.toml using a managed dependency
  const flowToml = `version = 1

[deps.hello]
pkg-path = "hello-dep"

[[tasks]]
name = "use-managed-dep"
command = "hello-dep"
description = "Confirm managed dep is used"
dependencies = ["hello"]
`;
  writeFileSync(join(base, "flow.toml"), flowToml, { encoding: "utf8" });

  const env = {
    ...process.env,
    PATH: `${fakeFlox}:${process.env.PATH}`,
    HOME: base,
    FLOX_NO_TELEMETRY: "1",
  };

  const cargo = spawnSync(
    "cargo",
    ["run", "--bin", "f", "--", "run", "use-managed-dep"],
    {
      cwd: base,
      env,
    }
  );
  assertOk(cargo, "cargo run f use-managed-dep");

  const output = cargo.stdout?.toString() ?? "";
  if (!output.includes("from-managed-env")) {
    throw new Error(`unexpected task output:\n${output}`);
  }

  console.log("deps e2e passed:\n" + output.trim());
}

main();

