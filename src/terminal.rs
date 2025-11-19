use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use which::which;

use crate::config::OptionsConfig;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const LOG_DIR_SUFFIX: &str = ".flow/tmux-logs";
const META_DIR_SUFFIX: &str = ".flow/tty-meta";
const SCRIPT_PATH_SUFFIX: &str = ".config/flow/tmux-enable-tracing.sh";
const FISH_CONF_SUFFIX: &str = ".config/fish/conf.d/flow-trace.fish";

pub fn maybe_enable_terminal_tracing(options: &OptionsConfig) {
    if !options.trace_terminal_io {
        return;
    }

    if let Err(err) = enforce_tmux_logging() {
        tracing::warn!(?err, "failed to enable tmux-based terminal tracing");
    }

    if let Err(err) = install_fish_hooks() {
        tracing::warn!(?err, "failed to install fish tracing hooks");
    }
}

fn enforce_tmux_logging() -> Result<()> {
    if which("tmux").is_err() {
        tracing::info!("tmux not found on PATH; skipping terminal IO tracing");
        return Ok(());
    }

    let home = home_dir();
    let log_dir = home.join(LOG_DIR_SUFFIX);
    fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create tmux log dir {}", log_dir.display()))?;

    let script_path = home.join(SCRIPT_PATH_SUFFIX);
    write_enable_script(&script_path, &log_dir)?;

    run_tmux(&["start-server"], "start tmux server for tracing")?;
    install_hooks(&script_path)?;
    prime_existing_panes(&script_path)?;

    tracing::info!(dir = %log_dir.display(), "tmux terminal tracing enabled");
    Ok(())
}

fn install_fish_hooks() -> Result<()> {
    if which("fish").is_err() {
        tracing::debug!("fish not found on PATH; skipping fish hook installation");
        return Ok(());
    }

    let home = home_dir();
    let meta_dir = home.join(META_DIR_SUFFIX);
    fs::create_dir_all(&meta_dir)
        .with_context(|| format!("failed to create fish meta dir {}", meta_dir.display()))?;

    let conf_path = home.join(FISH_CONF_SUFFIX);
    write_fish_conf(&conf_path, &meta_dir)?;
    Ok(())
}

fn install_hooks(script_path: &Path) -> Result<()> {
    let script_cmd = format!("run-shell {}", sh_quote(script_path));
    for hook in ["pane-add", "client-session-changed", "session-created"] {
        run_tmux(
            &["set-hook", "-g", hook, &script_cmd],
            "install tmux tracing hook",
        )?;
    }
    Ok(())
}

fn prime_existing_panes(script_path: &Path) -> Result<()> {
    let output = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_id}"])
        .output();

    let output = match output {
        Ok(out) if out.status.success() => out,
        Ok(_) => return Ok(()), // No panes yet; hooks will handle future ones.
        Err(err) => {
            tracing::warn!(?err, "unable to list tmux panes for tracing bootstrap");
            return Ok(());
        }
    };

    let script_cmd = sh_quote(script_path);
    for pane in String::from_utf8_lossy(&output.stdout).lines() {
        let pane = pane.trim();
        if pane.is_empty() {
            continue;
        }
        let run_shell_cmd = format!("{script_cmd} {pane}");
        run_tmux(&["run-shell", &run_shell_cmd], "prime tmux pane tracing")?;
    }

    Ok(())
}

fn write_enable_script(script_path: &Path, log_dir: &Path) -> Result<()> {
    if let Some(parent) = script_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create directory for tmux tracing script {}",
                parent.display()
            )
        })?;
    }

    let contents = format!(
        r#"#!/bin/sh
set -e
LOG_DIR={log_dir}
mkdir -p "$LOG_DIR"
TARGET="${{1:-!}}"
tmux pipe-pane -o -t "$TARGET" "cat >>${{LOG_DIR}}/pane-#{{session_name}}-#{{window_index}}-#{{pane_index}}.log"
"#,
        log_dir = sh_quote(log_dir)
    );
    fs::write(script_path, contents).with_context(|| {
        format!(
            "failed to write tmux tracing helper to {}",
            script_path.display()
        )
    })?;

    #[cfg(unix)]
    fs::set_permissions(script_path, fs::Permissions::from_mode(0o755)).with_context(|| {
        format!(
            "failed to mark tmux tracing script executable at {}",
            script_path.display()
        )
    })?;

    Ok(())
}

fn write_fish_conf(conf_path: &Path, meta_dir: &Path) -> Result<()> {
    const CONTENTS: &str = r#"if status --is-interactive
    if not set -q TMUX
        if not set -q FLOW_SKIP_AUTO_TMUX
            if type -q tmux
                set -l __flow_trace_tmux_session "flow"
                if set -q FLOW_AUTO_TMUX_SESSION
                    set __flow_trace_tmux_session $FLOW_AUTO_TMUX_SESSION
                end
                exec tmux new-session -A -s $__flow_trace_tmux_session
            end
        end
    end
end

set -g __flow_trace_meta_dir "%META_DIR%"
mkdir -p $__flow_trace_meta_dir

function __flow_trace_preexec --on-event fish_preexec
    set -l id (uuidgen)
    set -gx FLOW_CMD_ID $id
    set -l ts (date -Ins)
    set -l cmd (string join ' ' $argv)
    set -l pane (set -q TMUX_PANE; and echo $TMUX_PANE; or echo "nopane")
    set -l cwd (pwd)
    set -l cwd_b64 (printf "%s" $cwd | base64)
    set -l cmd_b64 (printf "%s" $cmd | base64)
    printf "\e]133;A;flow-cmd-start;%s\a" $id
    printf "start %s %s %s %s\n" $ts $id $cwd_b64 $cmd_b64 >> $__flow_trace_meta_dir/$pane.log
end

function __flow_trace_postexec --on-event fish_postexec
    set -l ts (date -Ins)
    set -l pane (set -q TMUX_PANE; and echo $TMUX_PANE; or echo "nopane")
    printf "\e]133;B;flow-cmd-end;%s;%s\a" $FLOW_CMD_ID $status
    printf "end %s %s %s\n" $ts $FLOW_CMD_ID $status >> $__flow_trace_meta_dir/$pane.log
end
"#;

    let rendered = CONTENTS.replace("%META_DIR%", &meta_dir.to_string_lossy());
    if let Some(parent) = conf_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create directory for fish tracing conf {}",
                parent.display()
            )
        })?;
    }

    // Avoid rewriting if unchanged to keep user shells happy.
    if let Ok(existing) = fs::read_to_string(conf_path) {
        if existing == rendered {
            return Ok(());
        }
    }

    fs::write(conf_path, rendered).with_context(|| {
        format!(
            "failed to write fish tracing hooks to {}",
            conf_path.display()
        )
    })
}

fn run_tmux(args: &[&str], context: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(args)
        .status()
        .with_context(|| format!("failed to execute tmux to {context}"))?;
    if status.success() {
        Ok(())
    } else {
        bail!(
            "tmux exited with status {} while attempting to {context}",
            status.code().unwrap_or(-1)
        );
    }
}

fn sh_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    let escaped = value.replace('\'', r"'\''");
    format!("'{escaped}'")
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
