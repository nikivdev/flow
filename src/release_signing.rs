use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STD};
use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

use crate::{
    cli::{ReleaseSigningAction, ReleaseSigningCommand, ReleaseSigningStoreOpts, ReleaseSigningSyncOpts},
    env,
};

const SIGNING_KEYS: [&str; 3] = ["MACOS_SIGN_P12_B64", "MACOS_SIGN_P12_PASSWORD", "MACOS_SIGN_IDENTITY"];

pub fn run(cmd: ReleaseSigningCommand) -> Result<()> {
    match cmd.action {
        ReleaseSigningAction::Status => status(),
        ReleaseSigningAction::Store(opts) => store(opts),
        ReleaseSigningAction::Sync(opts) => sync(opts),
    }
}

fn status() -> Result<()> {
    println!("macOS code signing (status)");
    println!("──────────────────────────");

    if cfg!(target_os = "macos") {
        let identities = list_codesign_identities().unwrap_or_default();
        let mut dev_id = Vec::new();
        let mut apple_dev = Vec::new();
        for name in identities {
            if name.starts_with("Developer ID Application:") {
                dev_id.push(name);
            } else if name.starts_with("Apple Development:") {
                apple_dev.push(name);
            }
        }

        if !dev_id.is_empty() {
            println!("Keychain: Developer ID Application identity found:");
            for name in dev_id {
                println!("  - {}", name);
            }
        } else if !apple_dev.is_empty() {
            println!("Keychain: no Developer ID Application identity found.");
            println!("Keychain: Apple Development identity found (not recommended for public distribution):");
            for name in apple_dev {
                println!("  - {}", name);
            }
            println!();
            println!("Next: create/download a Developer ID Application certificate (Apple Developer) and export it as .p12.");
        } else {
            println!("Keychain: no code signing identities found.");
        }
    } else {
        println!("Keychain: not on macOS (skipping).");
    }

    println!();

    // This may prompt for Touch ID if using cloud env store.
    match env::fetch_personal_env_vars(&SIGNING_KEYS.iter().map(|s| s.to_string()).collect::<Vec<_>>()) {
        Ok(vars) => {
            for key in SIGNING_KEYS {
                if let Some(value) = vars.get(key) {
                    // Avoid leaking secrets; show presence + size only.
                    println!("Env store: {} = set ({} bytes)", key, value.len());
                } else {
                    println!("Env store: {} = missing", key);
                }
            }
        }
        Err(err) => {
            println!("Env store: unable to read signing keys ({})", err);
            println!("Next: run `f env login` (cloud) and `f env unlock` (Touch ID), then retry.");
        }
    }

    println!();
    println!("GitHub: `f release signing sync` will copy env store values into GitHub Actions secrets via `gh`.");
    Ok(())
}

fn list_codesign_identities() -> Result<Vec<String>> {
    let output = Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .output()
        .context("failed to run `security find-identity`")?;
    if !output.status.success() {
        bail!("`security find-identity` failed");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    for line in text.lines() {
        // Example:
        //  1) <hash> "Developer ID Application: Name (TEAMID)"
        let Some(quoted) = line.split('"').nth(1) else { continue };
        let name = quoted.trim();
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

fn store(opts: ReleaseSigningStoreOpts) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("release signing store is only supported on macOS");
    }

    let p12_path = opts
        .p12
        .clone()
        .context("--p12 is required (path to exported .p12)")?;
    let p12_bytes = fs::read(&p12_path)
        .with_context(|| format!("failed to read p12 file at {}", p12_path.display()))?;

    let identity = opts.identity.clone().context("--identity is required")?;
    let password = opts
        .p12_password
        .clone()
        .context("--p12-password is required")?;

    if !identity.starts_with("Developer ID Application:") {
        eprintln!(
            "Warning: identity does not look like a Developer ID Application certificate: {}",
            identity
        );
    }

    let p12_b64 = BASE64_STD.encode(p12_bytes);

    if opts.dry_run {
        println!("[dry-run] Would set Flow personal env keys:");
        println!("  - MACOS_SIGN_P12_B64 ({} bytes)", p12_b64.len());
        println!("  - MACOS_SIGN_P12_PASSWORD ({} bytes)", password.len());
        println!("  - MACOS_SIGN_IDENTITY ({} bytes)", identity.len());
        return Ok(());
    }

    // Store in Flow personal env store (cloud if logged in; may prompt).
    env::set_personal_env_var("MACOS_SIGN_P12_B64", &p12_b64)?;
    env::set_personal_env_var("MACOS_SIGN_P12_PASSWORD", &password)?;
    env::set_personal_env_var("MACOS_SIGN_IDENTITY", &identity)?;

    println!("✓ Stored signing materials in Flow personal env store.");
    Ok(())
}

fn sync(opts: ReleaseSigningSyncOpts) -> Result<()> {
    let keys: Vec<String> = SIGNING_KEYS.iter().map(|k| k.to_string()).collect();
    let vars = env::fetch_personal_env_vars(&keys)
        .context("failed to read signing keys from Flow personal env store")?;

    if opts.dry_run {
        println!("[dry-run] Would set GitHub Actions secrets via `gh secret set`:");
        for key in SIGNING_KEYS {
            if vars.contains_key(key) {
                println!("  - {} (set in env store)", key);
            } else {
                println!("  - {} (missing in env store)", key);
            }
        }
        if let Some(repo) = opts.repo.as_deref() {
            println!("Repo: {}", repo);
        } else {
            println!("Repo: (from current directory)");
        }
        if SIGNING_KEYS.iter().any(|k| !vars.contains_key(*k)) {
            println!();
            println!("Next: set missing keys with `f release signing store ...`.");
        }
        return Ok(());
    }

    for key in SIGNING_KEYS {
        if !vars.contains_key(key) {
            bail!(
                "missing {} in Flow env store. Set it with `f release signing store ...` (or `f env set {}`) first.",
                key,
                key
            );
        }
    }

    ensure_gh_available()?;
    for key in SIGNING_KEYS {
        let value = vars.get(key).expect("checked above");
        gh_secret_set(opts.repo.as_deref(), key, value)?;
        println!("✓ Set GitHub secret: {}", key);
    }

    Ok(())
}

fn ensure_gh_available() -> Result<()> {
    let status = Command::new("gh")
        .args(["--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to run `gh` (GitHub CLI)")?;
    if !status.success() {
        bail!("`gh` is installed but not working");
    }
    Ok(())
}

fn gh_secret_set(repo: Option<&str>, name: &str, value: &str) -> Result<()> {
    let mut cmd = Command::new("gh");
    cmd.args(["secret", "set", name]);
    if let Some(repo) = repo {
        cmd.args(["--repo", repo]);
    }
    // Avoid passing secrets via argv (ps); `gh secret set` reads from stdin when --body is omitted.

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn `gh secret set {}`", name))?;

    {
        let stdin = child.stdin.as_mut().context("failed to open stdin for gh")?;
        stdin.write_all(value.as_bytes())?;
    }

    let status = child.wait()?;
    if !status.success() {
        bail!("`gh secret set {}` failed", name);
    }
    Ok(())
}
