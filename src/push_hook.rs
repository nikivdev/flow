use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::{PushHookEvalCommand, PushHooksAction, PushHooksCommand};
use crate::config;
use crate::push_policy::{self, PushContext};

const FLOW_PRE_PUSH_HOOK_MARKER: &str = "flow-global-pre-push-hook-v1";

pub fn run_hooks_command(cmd: PushHooksCommand) -> Result<()> {
    match cmd.action.unwrap_or(PushHooksAction::Status) {
        PushHooksAction::Install { force } => install_hooks(force),
        PushHooksAction::Uninstall => uninstall_hooks(),
        PushHooksAction::Status => print_hook_status(),
    }
}

pub fn run_hook_eval(cmd: PushHookEvalCommand) -> Result<()> {
    let repo_root = resolve_repo_root()?;
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("failed to read pre-push stdin")?;

    let policy = push_policy::load_merged_push_policy(&repo_root)?;
    let current_branch = current_branch(&repo_root)?;
    let updates = push_policy::parse_pre_push_updates(&input);
    let decision = push_policy::evaluate_pre_push(
        &PushContext {
            repo_root: repo_root.clone(),
            current_branch: current_branch.clone(),
            remote_name: cmd.remote_name.clone(),
            remote_url: cmd.remote_url.clone(),
            updates,
            orchestrated: env::var("FLOW_PUSH_ORCHESTRATED")
                .map(|value| value == "1")
                .unwrap_or(false),
        },
        &policy,
    );

    if !decision.allow {
        bail!(
            "{}",
            decision
                .message
                .unwrap_or_else(|| "push denied by Flow push policy".to_string())
        )
    }

    if decision.policy.run_prek {
        run_prek_validation(
            &repo_root,
            &policy,
            cmd.remote_name.as_deref(),
            cmd.remote_url.as_deref(),
            current_branch.as_deref(),
            decision.policy.home_branch.as_deref(),
        )?;
    }

    Ok(())
}

pub fn install_hooks(force: bool) -> Result<()> {
    let hooks_path = push_policy::effective_global_hooks_path()?;
    let current_hooks_path = current_global_hooks_path()?;

    if let Some(current) = current_hooks_path.as_ref()
        && normalize_path(current) != normalize_path(&hooks_path)
        && !force
    {
        bail!(
            "Global core.hooksPath already points to {}.\nRe-run with `f push hooks install --force` to overwrite it.",
            current.display()
        );
    }

    fs::create_dir_all(&hooks_path)
        .with_context(|| format!("failed to create {}", hooks_path.display()))?;
    let hook_path = hooks_path.join("pre-push");
    if hook_path.exists() && !is_flow_managed_hook(&hook_path)? && !force {
        bail!(
            "Refusing to overwrite non-Flow hook at {}.\nRe-run with `f push hooks install --force` to replace it.",
            hook_path.display()
        );
    }

    fs::write(&hook_path, render_pre_push_hook_script())
        .with_context(|| format!("failed to write {}", hook_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook_path, perms)?;
    }

    git_config_global_set("core.hooksPath", &hooks_path)?;
    println!("Installed Flow pre-push hook at {}", hook_path.display());
    println!("Global core.hooksPath -> {}", hooks_path.display());
    Ok(())
}

pub fn uninstall_hooks() -> Result<()> {
    let hooks_path = push_policy::effective_global_hooks_path()?;
    let hook_path = hooks_path.join("pre-push");
    let current_hooks_path = current_global_hooks_path()?;

    if hook_path.exists() && is_flow_managed_hook(&hook_path)? {
        fs::remove_file(&hook_path)
            .with_context(|| format!("failed to remove {}", hook_path.display()))?;
        println!("Removed Flow pre-push hook at {}", hook_path.display());
    } else if hook_path.exists() {
        bail!(
            "Refusing to remove non-Flow hook at {}.",
            hook_path.display()
        );
    } else {
        println!("No Flow pre-push hook found at {}", hook_path.display());
    }

    if let Some(current) = current_hooks_path
        && normalize_path(&current) == normalize_path(&hooks_path)
    {
        git_config_global_unset("core.hooksPath")?;
        println!("Unset global core.hooksPath");
    }

    Ok(())
}

pub fn print_hook_status() -> Result<()> {
    let hooks_path = push_policy::effective_global_hooks_path()?;
    let hook_path = hooks_path.join("pre-push");
    let current_hooks_path = current_global_hooks_path()?;
    let current_matches = current_hooks_path
        .as_ref()
        .map(|path| normalize_path(path) == normalize_path(&hooks_path))
        .unwrap_or(false);
    let hook_exists = hook_path.exists();
    let flow_managed = if hook_exists {
        is_flow_managed_hook(&hook_path)?
    } else {
        false
    };

    println!("Flow push hook status");
    println!("Expected hooks path: {}", hooks_path.display());
    match current_hooks_path {
        Some(path) => println!("Global core.hooksPath: {}", path.display()),
        None => println!("Global core.hooksPath: <unset>"),
    }
    println!(
        "Flow pre-push hook: {}",
        if hook_exists {
            hook_path.display().to_string()
        } else {
            format!("missing ({})", hook_path.display())
        }
    );
    println!("Flow-managed hook file: {}", yes_no(flow_managed));
    println!("Flow hooks path active: {}", yes_no(current_matches));
    Ok(())
}

fn current_global_hooks_path() -> Result<Option<PathBuf>> {
    let output = Command::new("git")
        .args(["config", "--global", "--get", "core.hooksPath"])
        .output()
        .context("failed to read global core.hooksPath")?;
    if !output.status.success() {
        return Ok(None);
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(config::expand_path(&value)))
}

fn git_config_global_set(key: &str, value: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["config", "--global", key, &value.to_string_lossy()])
        .status()
        .with_context(|| format!("failed to set git config {key}"))?;
    if !status.success() {
        bail!("git config --global {} failed", key);
    }
    Ok(())
}

fn git_config_global_unset(key: &str) -> Result<()> {
    let status = Command::new("git")
        .args(["config", "--global", "--unset", key])
        .status()
        .with_context(|| format!("failed to unset git config {key}"))?;
    if !status.success() {
        bail!("git config --global --unset {} failed", key);
    }
    Ok(())
}

fn resolve_repo_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to resolve git repo root")?;
    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }

    env::current_dir().context("failed to resolve current directory")
}

fn current_branch(repo_root: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["branch", "--show-current"])
        .output()
        .context("failed to query current branch")?;
    if !output.status.success() {
        return Ok(None);
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        Ok(None)
    } else {
        Ok(Some(branch))
    }
}

fn is_flow_managed_hook(path: &Path) -> Result<bool> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read hook {}", path.display()))?;
    Ok(content.contains(FLOW_PRE_PUSH_HOOK_MARKER))
}

fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn render_pre_push_hook_script() -> String {
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

# {marker}

resolve_flow_bin() {{
  if [[ -n "${{FLOW_PUSH_FLOW_BIN:-}}" && -x "${{FLOW_PUSH_FLOW_BIN}}" ]]; then
    printf '%s\n' "${{FLOW_PUSH_FLOW_BIN}}"
    return 0
  fi

  local candidate=""
  candidate="$(command -v f 2>/dev/null || true)"
  if [[ -n "$candidate" && -x "$candidate" ]]; then
    printf '%s\n' "$candidate"
    return 0
  fi

  for candidate in "$HOME/.flow/bin/f" "$HOME/bin/f" "$HOME/bin/f-bin"; do
    if [[ -x "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done

  candidate="$(command -v flow 2>/dev/null || true)"
  if [[ -n "$candidate" && -x "$candidate" ]]; then
    printf '%s\n' "$candidate"
    return 0
  fi

  echo "Flow push hook could not find the flow binary. Set FLOW_PUSH_FLOW_BIN or install f on PATH." >&2
  return 1
}}

same_path() {{
  if [[ -z "${{1:-}}" || -z "${{2:-}}" ]]; then
    return 1
  fi

  local left_dir=""
  local right_dir=""
  left_dir="$(cd "$(dirname "$1")" 2>/dev/null && pwd -P)" || return 1
  right_dir="$(cd "$(dirname "$2")" 2>/dev/null && pwd -P)" || return 1
  [[ "$left_dir/$(basename "$1")" == "$right_dir/$(basename "$2")" ]]
}}

payload_file="$(mktemp "${{TMPDIR:-/tmp}}/flow-pre-push.XXXXXX")"
cleanup() {{
  rm -f "$payload_file"
}}
trap cleanup EXIT

cat >"$payload_file"

flow_bin="$(resolve_flow_bin)"
"$flow_bin" push hook-eval --remote-name "${{1:-}}" --remote-url "${{2:-}}" <"$payload_file"

git_dir="$(git rev-parse --absolute-git-dir 2>/dev/null || true)"
legacy_hook=""
if [[ -n "$git_dir" ]]; then
  legacy_hook="$git_dir/hooks/pre-push"
fi

if same_path "${{legacy_hook:-}}" "$0"; then
  legacy_hook=""
fi

if [[ -n "${{legacy_hook:-}}" && -x "$legacy_hook" && "${{FLOW_PUSH_HOOK_CHAINED:-0}}" != "1" ]]; then
  FLOW_PUSH_HOOK_CHAINED=1 "$legacy_hook" "$@" <"$payload_file"
fi
"#,
        marker = FLOW_PRE_PUSH_HOOK_MARKER,
    )
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn run_prek_validation(
    repo_root: &Path,
    policy: &push_policy::EffectivePushPolicyConfig,
    remote_name: Option<&str>,
    remote_url: Option<&str>,
    current_branch: Option<&str>,
    home_branch: Option<&str>,
) -> Result<()> {
    let prek_bin = policy
        .prek_bin
        .clone()
        .unwrap_or_else(|| PathBuf::from("prek"));
    let mut cmd = Command::new(&prek_bin);
    cmd.current_dir(repo_root)
        .arg("run")
        .arg("--stage")
        .arg("pre-push")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .env("FLOW_PUSH_REPO_ROOT", repo_root)
        .env(
            "FLOW_PUSH_ORCHESTRATED",
            env::var("FLOW_PUSH_ORCHESTRATED").unwrap_or_else(|_| "0".to_string()),
        );

    if let Some(value) = remote_name
        && !value.trim().is_empty()
    {
        cmd.env("FLOW_PUSH_REMOTE_NAME", value);
    }
    if let Some(value) = remote_url
        && !value.trim().is_empty()
    {
        cmd.env("FLOW_PUSH_REMOTE_URL", value);
    }
    if let Some(branch) = current_branch
        && !branch.trim().is_empty()
    {
        cmd.env("FLOW_PUSH_CURRENT_BRANCH", branch);
    }
    if let Some(branch) = home_branch
        && !branch.trim().is_empty()
    {
        cmd.env("FLOW_PUSH_HOME_BRANCH", branch);
    }

    let status = cmd.status().with_context(|| {
        format!(
            "failed to run prek pre-push validation via {}",
            prek_bin.display()
        )
    })?;
    if status.success() {
        return Ok(());
    }

    bail!("prek pre-push validation failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_hook_script_mentions_internal_eval_command() {
        let script = render_pre_push_hook_script();
        assert!(script.contains("push hook-eval"));
        assert!(script.contains(FLOW_PRE_PUSH_HOOK_MARKER));
        assert!(script.contains("legacy_hook"));
        assert!(script.contains("same_path"));
    }
}
