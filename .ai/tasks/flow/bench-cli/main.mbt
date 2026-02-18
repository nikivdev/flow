// title: Flow CLI Bench
// description: Quick CLI benchmark for high-frequency Flow entry points.
// tags: [flow,bench,latency]

fn project_root() -> String {
  match @sys.get_env_var("FLOW_AI_TASK_PROJECT_ROOT") {
    Some(root) => root
    None => "../../../.."
  }
}

async fn run_step(label : String, command : String, root : String) -> Unit raise {
  println("==> " + label)
  println("    " + command)
  let code = @process.run(
    "sh",
    ["-lc", command],
    stdin=@stdio.stdin,
    stdout=@stdio.stdout,
    stderr=@stdio.stderr,
    cwd=root,
  )
  if code != 0 {
    abort("step failed: " + label + " (exit " + code.to_string() + ")")
  }
}

async fn main raise {
  let root = project_root()
  let bench_script =
    "set -euo pipefail; " +
    "bin=${FLOW_BIN:-./target/release/f}; " +
    "if [ ! -x \"$bin\" ]; then bin=./target/debug/f; fi; " +
    "runs=${FLOW_BENCH_ITERATIONS:-20}; " +
    "warmup=${FLOW_BENCH_WARMUP:-5}; " +
    "echo \"bench bin: $bin\"; " +
    "echo \"runs: $runs warmup: $warmup\"; " +
    "if command -v hyperfine >/dev/null 2>&1; then " +
    "hyperfine --warmup \"$warmup\" --runs \"$runs\" " +
    "\"$bin --help >/dev/null\" " +
    "\"$bin tasks list >/dev/null\" " +
    "\"$bin tasks dupes >/dev/null\"; " +
    "else " +
    "echo 'hyperfine not found; fallback single-run timing'; " +
    "for cmd in '--help' 'tasks list' 'tasks dupes'; do " +
    "echo \"timing: $bin $cmd\"; /usr/bin/time -l sh -lc \"$bin $cmd >/dev/null\"; " +
    "done; " +
    "fi"

  run_step("cli benchmark", bench_script, root)
  println("flow/bench-cli: ok")
}
