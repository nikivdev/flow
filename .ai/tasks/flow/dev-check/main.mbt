// title: Flow Dev Check
// description: Fast local quality gate for Flow code changes.
// tags: [flow,dev,check]

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
  run_step("cargo check", "cargo check --all-targets", root)
  run_step(
    "targeted ai task test",
    "cargo test ai_tasks::tests::parses_metadata_comments -- --nocapture",
    root,
  )
  run_step("tasks help smoke", "./target/debug/f tasks --help >/dev/null", root)
  println("flow/dev-check: ok")
}
