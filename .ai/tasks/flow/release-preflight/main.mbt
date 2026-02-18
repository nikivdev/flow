// title: Flow Release Preflight
// description: Build release binary and validate core release smoke checks.
// tags: [flow,release,preflight]

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

  run_step("build release f", "cargo build --release --bin f", root)
  run_step("release version", "./target/release/f --version", root)
  run_step("release tasks help", "./target/release/f tasks --help >/dev/null", root)
  run_step(
    "release regression smoke",
    "FLOW_BIN=./target/release/f ./target/release/f ai:flow/regression-smoke",
    root,
  )

  println("flow/release-preflight: ok")
}
