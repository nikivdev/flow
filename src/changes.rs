use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ai, config};
use crate::cli::{ChangesAction, ChangesCommand, DiffCommand};

pub fn run(cmd: ChangesCommand) -> Result<()> {
    match cmd.action {
        Some(ChangesAction::CurrentDiff) => {
            print_current_diff()?;
        }
        Some(ChangesAction::Accept { diff, file }) => {
            apply_diff(diff, file)?;
        }
        None => {
            bail!("Missing changes subcommand. Use: f changes current-diff | f changes accept <diff>");
        }
    }
    Ok(())
}

pub fn run_diff(cmd: DiffCommand) -> Result<()> {
    match cmd.hash {
        Some(hash) => unroll_bundle(&hash),
        None => create_bundle(),
    }
}

fn repo_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to run git rev-parse")?;
    if !output.status.success() {
        bail!("Not a git repository.");
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        bail!("Unable to resolve git root.");
    }
    Ok(PathBuf::from(root))
}

fn git_output_in(repo_root: &Path, args: &[&str]) -> Result<(String, bool)> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let ok = output.status.success();
    if !ok && stdout.is_empty() {
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok((stdout, ok))
}

fn git_ref_exists(repo_root: &Path, reference: &str) -> Result<bool> {
    let full_ref = format!("{}^{{commit}}", reference);
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["rev-parse", "--verify", &full_ref])
        .output()
        .with_context(|| format!("failed to verify git ref {}", reference))?;
    Ok(output.status.success())
}

fn resolve_base_ref(repo_root: &Path) -> Result<String> {
    let candidates = ["main", "origin/main", "master", "origin/master"];
    for candidate in candidates {
        if git_ref_exists(repo_root, candidate)? {
            return Ok(candidate.to_string());
        }
    }
    Ok("HEAD".to_string())
}

fn list_untracked(repo_root: &Path) -> Result<Vec<String>> {
    let (status, _ok) = git_output_in(repo_root, &["status", "--porcelain"])?;
    let mut untracked = Vec::new();
    for line in status.lines() {
        if let Some(path) = line.strip_prefix("?? ") {
            if !path.trim().is_empty() {
                untracked.push(path.trim().to_string());
            }
        }
    }
    Ok(untracked)
}

fn print_current_diff() -> Result<()> {
    let repo_root = repo_root()?;
    let base_ref = resolve_base_ref(&repo_root)?;
    let diff = diff_from_base(&repo_root, &base_ref)?;

    print!("{}", diff);
    Ok(())
}

fn diff_from_base(repo_root: &Path, base_ref: &str) -> Result<String> {
    let (tracked_diff, _ok) = git_output_in(&repo_root, &["diff", "--binary", base_ref])?;
    let mut diff = tracked_diff;

    for path in list_untracked(&repo_root)? {
        let (patch, _ok) = git_output_in(
            &repo_root,
            &["diff", "--no-index", "--binary", "--", "/dev/null", &path],
        )?;
        diff.push_str(&patch);
    }

    Ok(diff)
}

fn read_diff_input(diff: Option<String>, file: Option<PathBuf>) -> Result<String> {
    if let Some(file) = file {
        return fs::read_to_string(&file)
            .with_context(|| format!("failed to read diff file {}", file.display()));
    }

    if let Some(raw) = diff {
        if raw == "-" {
            return read_stdin();
        }
        let as_path = PathBuf::from(&raw);
        if as_path.exists() {
            return fs::read_to_string(&as_path)
                .with_context(|| format!("failed to read diff file {}", as_path.display()));
        }
        return Ok(raw);
    }

    if atty::is(atty::Stream::Stdin) {
        bail!("No diff provided. Pass a diff string, a file path, or '-' to read stdin.");
    }

    read_stdin()
}

fn read_stdin() -> Result<String> {
    let mut buffer = String::new();
    io::stdin()
        .read_to_string(&mut buffer)
        .context("failed to read diff from stdin")?;
    Ok(buffer)
}

fn apply_diff(diff: Option<String>, file: Option<PathBuf>) -> Result<()> {
    let repo_root = repo_root()?;
    let content = read_diff_input(diff, file)?;
    if content.trim().is_empty() {
        bail!("Diff input is empty.");
    }

    let mut child = Command::new("git")
        .current_dir(&repo_root)
        .args(["apply", "--whitespace=fix", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to run git apply")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(content.as_bytes())
            .context("failed to write diff to git apply")?;
    }

    let status = child.wait().context("failed to wait for git apply")?;
    if !status.success() {
        bail!("git apply failed");
    }

    println!("Applied diff successfully.");
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct DiffBundle {
    hash: String,
    version: u32,
    created_at: String,
    repo_root: String,
    base_ref: String,
    diff: String,
    ai_sessions: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct DiffBundlePayload {
    version: u32,
    created_at: String,
    repo_root: String,
    base_ref: String,
    diff: String,
    ai_sessions: Vec<serde_json::Value>,
}

fn create_bundle() -> Result<()> {
    let repo_root = repo_root()?;
    let base_ref = resolve_base_ref(&repo_root)?;
    let diff = diff_from_base(&repo_root, &base_ref)?;
    let ai_sessions = match ai::get_sessions_for_gitedit(&repo_root) {
        Ok(sessions) => sessions
            .into_iter()
            .filter_map(|session| serde_json::to_value(session).ok())
            .collect(),
        Err(err) => {
            eprintln!("Warning: failed to collect AI sessions: {}", err);
            Vec::new()
        }
    };
    let created_at = Utc::now().to_rfc3339();
    let repo_root_str = repo_root.display().to_string();

    let payload = DiffBundlePayload {
        version: 1,
        created_at: created_at.clone(),
        repo_root: repo_root_str.clone(),
        base_ref: base_ref.clone(),
        diff: diff.clone(),
        ai_sessions: ai_sessions.clone(),
    };

    let hash = bundle_hash(&payload)?;
    let bundle = DiffBundle {
        hash: hash.clone(),
        version: payload.version,
        created_at: payload.created_at,
        repo_root: payload.repo_root,
        base_ref: payload.base_ref,
        diff: payload.diff,
        ai_sessions: payload.ai_sessions,
    };

    let bundle_path = write_bundle(&bundle)?;

    println!("Diff hash: {}", hash);
    println!("Base ref: {}", base_ref);
    println!("AI sessions: {}", ai_sessions.len());
    println!("Bundle: {}", bundle_path.display());
    println!("Unroll: f diff {}", hash);

    Ok(())
}

fn unroll_bundle(id: &str) -> Result<()> {
    let (bundle, source_path) = read_bundle(id)?;
    let repo_root = repo_root()?;
    let output_dir = repo_root.join(".ai").join("diffs").join(&bundle.hash);
    fs::create_dir_all(&output_dir)?;

    let diff_path = output_dir.join("diff.patch");
    fs::write(&diff_path, &bundle.diff)
        .with_context(|| format!("failed to write {}", diff_path.display()))?;

    let sessions_path = output_dir.join("sessions.json");
    let sessions_json = serde_json::to_string_pretty(&bundle.ai_sessions)
        .context("failed to serialize AI sessions")?;
    fs::write(&sessions_path, sessions_json)
        .with_context(|| format!("failed to write {}", sessions_path.display()))?;

    let meta = serde_json::json!({
        "hash": bundle.hash,
        "version": bundle.version,
        "created_at": bundle.created_at,
        "repo_root": bundle.repo_root,
        "base_ref": bundle.base_ref,
        "session_count": bundle.ai_sessions.len(),
        "diff_bytes": bundle.diff.as_bytes().len(),
        "source_bundle": source_path.as_ref().map(|p| p.display().to_string()),
    });
    let meta_path = output_dir.join("meta.json");
    fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)
        .with_context(|| format!("failed to write {}", meta_path.display()))?;

    println!("Unrolled diff {} -> {}", bundle.hash, output_dir.display());
    if let Some(path) = source_path {
        println!("Source bundle: {}", path.display());
    }
    println!("Apply: f changes accept --file {}", diff_path.display());

    Ok(())
}

fn bundle_hash(payload: &DiffBundlePayload) -> Result<String> {
    let bytes = serde_json::to_vec(payload).context("failed to serialize diff bundle")?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    Ok(hex::encode(digest))
}

fn bundle_dir() -> Result<PathBuf> {
    let config_dir = config::ensure_global_config_dir()?;
    let diffs_dir = config_dir.join("diffs");
    fs::create_dir_all(&diffs_dir)?;
    Ok(diffs_dir)
}

fn write_bundle(bundle: &DiffBundle) -> Result<PathBuf> {
    let diffs_dir = bundle_dir()?;
    let path = diffs_dir.join(format!("{}.json", bundle.hash));
    let payload = serde_json::to_string_pretty(bundle).context("failed to serialize bundle")?;
    fs::write(&path, payload).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

fn read_bundle(id: &str) -> Result<(DiffBundle, Option<PathBuf>)> {
    let candidate = PathBuf::from(id);
    let path = if candidate.exists() {
        candidate
    } else {
        bundle_dir()?.join(format!("{}.json", id))
    };

    if !path.exists() {
        bail!(
            "Diff bundle not found. Expected {} or pass a path to a bundle file.",
            path.display()
        );
    }

    let data = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let bundle: DiffBundle = serde_json::from_str(&data)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let payload = DiffBundlePayload {
        version: bundle.version,
        created_at: bundle.created_at.clone(),
        repo_root: bundle.repo_root.clone(),
        base_ref: bundle.base_ref.clone(),
        diff: bundle.diff.clone(),
        ai_sessions: bundle.ai_sessions.clone(),
    };
    let expected = bundle_hash(&payload)?;
    if expected != bundle.hash {
        eprintln!(
            "Warning: bundle hash mismatch (expected {}, got {}).",
            expected, bundle.hash
        );
    }

    Ok((bundle, Some(path)))
}
