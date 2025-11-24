use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::FloxInstallSpec;

const MANIFEST_VERSION: u8 = 1;
const ENV_VERSION: u8 = 1;

/// Paths needed to invoke `flox activate` for a generated manifest.
#[derive(Clone, Debug)]
pub struct FloxEnv {
    pub project_root: PathBuf,
    pub manifest_path: PathBuf,
    pub lockfile_path: PathBuf,
}

#[derive(Serialize)]
struct ManifestFile {
    version: u8,
    install: BTreeMap<String, FloxInstallSpec>,
}

#[derive(Serialize)]
struct EnvJson {
    version: u8,
    manifest: String,
    lockfile: String,
}

/// Ensure a flox manifest exists for the given packages and return the paths to use.
pub fn ensure_env(project_root: &Path, packages: &[(String, FloxInstallSpec)]) -> Result<FloxEnv> {
    if packages.is_empty() {
        bail!("flox environment requested without any packages");
    }

    let flox_bin = which::which("flox")
        .context("flox is required to use [deps]; install flox and ensure it is on PATH")?;

    let env_dir = project_root.join(".flox").join("env");
    let manifest_path = env_dir.join("manifest.toml");
    let lockfile_path = env_dir.join("manifest.lock");
    fs::create_dir_all(&env_dir)
        .with_context(|| format!("failed to create flox env directory {}", env_dir.display()))?;

    let manifest_toml = render_manifest(packages)?;
    let manifest_changed = write_if_changed(&manifest_path, &manifest_toml)?;

    // Produce a lockfile so flox activations don't need to mutate state.
    if manifest_changed || !lockfile_path.exists() {
        let output = Command::new(&flox_bin)
            .arg("lock-manifest")
            .arg(&manifest_path)
            .output()
            .with_context(|| "failed to run 'flox lock-manifest'")?;

        if output.status.success() {
            write_if_changed(
                &lockfile_path,
                String::from_utf8_lossy(&output.stdout).as_ref(),
            )?;
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("flox lock-manifest failed: {}", stderr.trim());
        }
    }

    write_env_json(project_root, &manifest_path, &lockfile_path)?;

    Ok(FloxEnv {
        project_root: project_root.to_path_buf(),
        manifest_path,
        lockfile_path,
    })
}

/// Run a shell command inside the prepared flox environment.
pub fn run_in_env(env: &FloxEnv, workdir: &Path, command: &str) -> Result<()> {
    write_env_json(&env.project_root, &env.manifest_path, &env.lockfile_path)?;

    let flox_bin = which::which("flox").context("flox is required to run tasks with flox deps")?;
    let status = Command::new(&flox_bin)
        .arg("activate")
        .arg("-d")
        .arg(&env.project_root)
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .status()
        .with_context(|| "failed to spawn flox activate for task")?;

    if status.success() {
        return Ok(());
    }

    tracing::debug!(
        status = ?status.code(),
        "flox activate failed; running task with host PATH"
    );
    run_on_host(workdir, command)
}

fn write_env_json(project_root: &Path, manifest_path: &Path, lockfile_path: &Path) -> Result<()> {
    let flox_root = project_root.join(".flox");
    let top_level = flox_root.join("env.json");
    let nested = flox_root.join("env").join("env.json");

    let nested_json = EnvJson {
        version: ENV_VERSION,
        manifest: manifest_path.to_string_lossy().to_string(),
        lockfile: lockfile_path.to_string_lossy().to_string(),
    };
    // top-level env.json with relative paths for flox CLI expectations
    let top_level_json = EnvJson {
        version: ENV_VERSION,
        manifest: "env/manifest.toml".to_string(),
        lockfile: "env/manifest.lock".to_string(),
    };

    if let Some(parent) = top_level.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if let Some(parent) = nested.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let top_level_contents = serde_json::to_string_pretty(&top_level_json)
        .context("failed to render top-level env.json")?;
    let nested_contents =
        serde_json::to_string_pretty(&nested_json).context("failed to render nested env.json")?;

    write_if_changed(&top_level, &top_level_contents)?;
    write_if_changed(&nested, &nested_contents)?;
    Ok(())
}

fn run_on_host(workdir: &Path, command: &str) -> Result<()> {
    let host_status = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .status()
        .with_context(|| "failed to spawn command without managed env")?;
    if host_status.success() {
        Ok(())
    } else {
        bail!(
            "command exited with status {}",
            host_status.code().unwrap_or(-1)
        );
    }
}

fn render_manifest(packages: &[(String, FloxInstallSpec)]) -> Result<String> {
    let mut install = BTreeMap::new();
    for (name, spec) in packages {
        install.insert(name.clone(), spec.clone());
    }

    let manifest = ManifestFile {
        version: MANIFEST_VERSION,
        install,
    };

    toml::to_string_pretty(&manifest).context("failed to render flox manifest")
}

fn write_if_changed(path: &Path, contents: &str) -> Result<bool> {
    let needs_write = fs::read_to_string(path).map_or(true, |existing| existing != contents);
    if needs_write {
        fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(needs_write)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_renders_with_full_descriptor() {
        let deps = vec![(
            "ripgrep".to_string(),
            FloxInstallSpec {
                pkg_path: "ripgrep".into(),
                pkg_group: Some("tools".into()),
                version: Some("14".into()),
                systems: Some(vec!["x86_64-darwin".into()]),
                priority: Some(10),
            },
        )];

        let rendered = render_manifest(&deps).expect("render manifest");
        assert!(rendered.contains("version = 1"));
        assert!(rendered.contains("[install.ripgrep]"));
        assert!(rendered.contains(r#"pkg-path = "ripgrep""#));
        assert!(rendered.contains(r#"pkg-group = "tools""#));
        assert!(rendered.contains(r#"version = "14""#));
        assert!(rendered.contains(r#"priority = 10"#));
    }
}
