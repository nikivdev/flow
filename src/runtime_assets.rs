use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

const FLOW_RUNTIME_ASSETS_ROOT_ENV: &str = "FLOW_RUNTIME_ASSETS_ROOT";

pub fn installed_assets_root_for_exe(exe: &Path) -> Option<PathBuf> {
    let bin_dir = exe.parent()?;
    let install_root = bin_dir.parent()?;
    Some(install_root.join("share").join("flow"))
}

pub fn asset_path(relative: &str) -> Option<PathBuf> {
    let relative = sanitize_relative(relative);
    candidate_roots()
        .into_iter()
        .map(|root| root.join(relative))
        .find(|candidate| candidate.exists())
}

pub fn require_asset_path(relative: &str) -> Result<PathBuf> {
    if let Some(path) = asset_path(relative) {
        return Ok(path);
    }

    let searched = candidate_roots()
        .into_iter()
        .map(|root| root.join(sanitize_relative(relative)).display().to_string())
        .collect::<Vec<_>>();
    bail!(
        "runtime asset '{}' not found. searched:\n{}",
        relative,
        searched
            .iter()
            .map(|path| format!("- {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn sanitize_relative(relative: &str) -> &Path {
    Path::new(relative.trim_start_matches('/'))
}

fn candidate_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Some(root) = env::var_os(FLOW_RUNTIME_ASSETS_ROOT_ENV).map(PathBuf::from) {
        push_unique(&mut roots, root);
    }

    if let Ok(exe) = env::current_exe() {
        if let Some(root) = installed_assets_root_for_exe(&exe) {
            push_unique(&mut roots, root);
        }
        if let Ok(canonical) = exe.canonicalize()
            && let Some(root) = installed_assets_root_for_exe(&canonical)
        {
            push_unique(&mut roots, root);
        }
    }

    push_unique(&mut roots, PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    roots
}

fn push_unique(roots: &mut Vec<PathBuf>, root: PathBuf) {
    if !roots.iter().any(|candidate| candidate == &root) {
        roots.push(root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let previous = env::var_os(key);
            unsafe {
                env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe {
                    env::set_var(self.key, value);
                },
                None => unsafe {
                    env::remove_var(self.key);
                },
            }
        }
    }

    #[test]
    fn installed_assets_root_uses_sibling_share_flow() {
        let exe = Path::new("/tmp/flow/bin/f");
        assert_eq!(
            installed_assets_root_for_exe(exe),
            Some(PathBuf::from("/tmp/flow/share/flow"))
        );
    }

    #[test]
    fn env_override_takes_precedence() {
        let dir = tempdir().expect("tempdir");
        let asset_root = dir.path().join("assets");
        let asset = asset_root.join("scripts").join("private_mirror.py");
        std::fs::create_dir_all(asset.parent().expect("asset parent")).expect("mkdirs");
        std::fs::write(&asset, "#!/usr/bin/env python3\n").expect("write asset");
        let _guard = EnvVarGuard::set(FLOW_RUNTIME_ASSETS_ROOT_ENV, &asset_root);

        let resolved = asset_path("scripts/private_mirror.py").expect("resolve asset");

        assert_eq!(resolved, asset);
    }
}
