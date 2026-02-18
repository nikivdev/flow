// title: Flow Regression Smoke
// description: Validate task discovery and AI task execution in a fresh temp project.
// tags: [flow,smoke,regression]

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
  let smoke_script =
    "set -euo pipefail; " +
    "bin=${FLOW_BIN:-$PWD/target/debug/f}; " +
    "case \"$bin\" in /*) ;; *) bin=\"$PWD/$bin\" ;; esac; " +
    "if [ ! -x \"$bin\" ]; then bin=$PWD/target/release/f; fi; " +
    "tmp=$(mktemp -d /tmp/flow-task-smoke.XXXXXX); " +
    "trap 'rm -rf \"$tmp\"' EXIT; " +
    "printf '%s\\n' '[[tasks]]' 'name = \"hello\"' 'command = \"echo hello-flow\"' > \"$tmp/flow.toml\"; " +
    "(cd \"$tmp\" && \"$bin\" tasks init-ai --root . >/dev/null); " +
    "(cd \"$tmp\" && \"$bin\" tasks list | grep -q 'ai:starter'); " +
    "(cd \"$tmp\" && out=$(\"$bin\" starter) && case \"$out\" in *'starter ai task: ok'*) : ;; *) echo 'starter output mismatch' >&2; exit 1 ;; esac); " +
    "(cd \"$tmp\" && out=$(\"$bin\" hello) && case \"$out\" in *'hello-flow'*) : ;; *) echo 'hello output mismatch' >&2; exit 1 ;; esac); " +
    "echo 'regression smoke passed'"

  run_step("temp project smoke", smoke_script, root)
  println("flow/regression-smoke: ok")
}
