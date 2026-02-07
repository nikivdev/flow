use std::env;
use std::io::IsTerminal;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::cli::HashOpts;
use crate::env as flow_env;

const LINK_PREFIX: &str = "unstash./";

pub fn run(opts: HashOpts) -> Result<()> {
    if opts.args.is_empty() {
        bail!("Usage: f hash <paths or unhash args>");
    }

    let unhash_bin = which::which("unhash")
        .context("unhash not found on PATH. Run `f deploy-unhash` in the unhash repo.")?;

    let mut cmd = Command::new(unhash_bin);
    cmd.args(&opts.args);

    if env::var("UNHASH_KEY").is_err() {
        if let Ok(Some(value)) = flow_env::get_personal_env_var("UNHASH_KEY") {
            cmd.env("UNHASH_KEY", value);
        }
    }

    let output = cmd.output().context("failed to run unhash")?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("unhash failed: {}\n{}{}", output.status, stdout, stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines().filter(|line| !line.trim().is_empty());
    let hash = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("unhash output missing hash"))?
        .trim()
        .to_string();

    let link = format!("{LINK_PREFIX}{hash}");
    copy_to_clipboard(&link)?;

    println!("{hash}");
    println!("{link}");

    if let Some(path_line) = lines.next() {
        println!("{}", path_line.trim());
    }

    Ok(())
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    if std::env::var("FLOW_NO_CLIPBOARD").is_ok() || !std::io::stdin().is_terminal() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let mut child = Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("failed to spawn pbcopy")?;

        if let Some(stdin) = child.stdin.as_mut() {
            use std::io::Write;
            stdin.write_all(text.as_bytes())?;
        }

        child.wait()?;
    }

    #[cfg(target_os = "linux")]
    {
        let result = Command::new("xclip")
            .arg("-selection")
            .arg("clipboard")
            .stdin(std::process::Stdio::piped())
            .spawn();

        let mut child = match result {
            Ok(c) => c,
            Err(_) => Command::new("xsel")
                .arg("--clipboard")
                .arg("--input")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .context("failed to spawn xclip or xsel")?,
        };

        if let Some(stdin) = child.stdin.as_mut() {
            use std::io::Write;
            stdin.write_all(text.as_bytes())?;
        }

        child.wait()?;
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        bail!("clipboard not supported on this platform");
    }

    Ok(())
}
