// title: Flow PR Ready
// description: Pre-PR gate: compile, task checks, docs parity, gitignore hygiene.
// tags: [flow,pr,gate]

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

  run_step("dev check", "./target/debug/f ai:flow/dev-check", root)

  run_step(
    "docs parity gate",
    "set -euo pipefail; " +
    "src_changed=$(git diff --name-only HEAD -- src/cli.rs src/tasks.rs src/palette.rs src/ai_tasks.rs || true); " +
    "if [ -n \"$src_changed\" ]; then " +
    "docs_changed=$(git diff --name-only HEAD -- docs/commands/readme.md docs/commands/tasks.md docs/commands/recipe.md || true); " +
    "if [ -z \"$docs_changed\" ]; then echo 'expected docs/commands updates for CLI/task changes' >&2; exit 1; fi; " +
    "fi; " +
    "echo 'docs parity: ok'",
    root,
  )

  run_step(
    "gitignore hygiene gate",
    "set -euo pipefail; " +
    "if git diff -- .gitignore | grep -E '\\.beads/|\\.rise/|\\.ai/todos/\\*\\.bike' >/dev/null; then " +
    "echo 'blocked personal tooling pattern detected in .gitignore diff' >&2; exit 1; " +
    "fi; " +
    "echo 'gitignore hygiene: ok'",
    root,
  )

  println("flow/pr-ready: ok")
}
