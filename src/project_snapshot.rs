use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{ai_tasks, discover};

const SNAPSHOT_CACHE_VERSION: u32 = 1;
const SNAPSHOT_CACHE_ENV_DISABLE: &str = "FLOW_DISABLE_DISCOVERY_CACHE";

#[derive(Debug, Clone)]
pub struct ProjectSnapshot {
    pub root: PathBuf,
    pub discovery: discover::DiscoveryResult,
    pub ai_tasks: Vec<ai_tasks::DiscoveredAiTask>,
}

impl ProjectSnapshot {
    pub fn from_root_tasks_only(root: &Path) -> Result<Self> {
        let root = canonicalize_root(root)?;
        Self::from_canonical_root_tasks_only(root)
    }

    pub fn from_task_config(config: &Path, climb_to_default_flow_toml: bool) -> Result<Self> {
        let root = resolve_project_root_from_config(config, climb_to_default_flow_toml)?;
        Self::from_canonical_root(root)
    }

    pub fn from_task_config_tasks_only(
        config: &Path,
        climb_to_default_flow_toml: bool,
    ) -> Result<Self> {
        let root = resolve_project_root_from_config(config, climb_to_default_flow_toml)?;
        Self::from_canonical_root_tasks_only(root)
    }

    pub fn from_current_dir(climb_to_flow_toml: bool) -> Result<Self> {
        let root = resolve_project_root_from_current_dir(climb_to_flow_toml)?;
        Self::from_canonical_root(root)
    }

    pub fn has_any_tasks(&self) -> bool {
        !self.discovery.tasks.is_empty() || !self.ai_tasks.is_empty()
    }

    pub(crate) fn from_canonical_root(root: PathBuf) -> Result<Self> {
        let (discovery, ai_tasks) = load_or_build_project_sections(&root, true)?;
        Ok(Self {
            root,
            discovery,
            ai_tasks,
        })
    }

    pub(crate) fn from_canonical_root_tasks_only(root: PathBuf) -> Result<Self> {
        let (discovery, _) = load_or_build_project_sections(&root, false)?;
        Ok(Self {
            root,
            discovery,
            ai_tasks: Vec::new(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct AiTaskSnapshot {
    pub root: PathBuf,
    pub tasks: Vec<ai_tasks::DiscoveredAiTask>,
}

impl AiTaskSnapshot {
    pub fn from_root(root: &Path) -> Result<Self> {
        let root = canonicalize_root(root)?;
        Self::from_canonical_root(root)
    }

    pub(crate) fn from_canonical_root(root: PathBuf) -> Result<Self> {
        let tasks = load_or_build_ai_tasks(&root)?;
        Ok(Self { root, tasks })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SnapshotCacheEntry {
    version: u32,
    discovery: Option<CachedDiscoverySection>,
    ai_tasks: Option<CachedAiTasksSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedDiscoverySection {
    result: discover::DiscoveryResult,
    watched: Vec<PathStamp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedAiTasksSection {
    tasks: Vec<ai_tasks::DiscoveredAiTask>,
    watched: Vec<PathStamp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PathStamp {
    path: PathBuf,
    is_dir: bool,
    len: u64,
    modified_sec: u64,
    modified_nsec: u32,
}

fn load_or_build_project_sections(
    root: &Path,
    include_ai_tasks: bool,
) -> Result<(discover::DiscoveryResult, Vec<ai_tasks::DiscoveredAiTask>)> {
    if cache_disabled() {
        let discovery = discover::discover_tasks_from_root(root.to_path_buf())?;
        let ai_tasks = if include_ai_tasks {
            ai_tasks::discover_tasks_from_root(root.to_path_buf())?
        } else {
            Vec::new()
        };
        return Ok((discovery, ai_tasks));
    }

    let cache_path = snapshot_cache_path(root);
    let mut cache = read_cache_entry(&cache_path).unwrap_or_default();
    let mut cache_dirty = false;

    let discovery = match cache.discovery.as_ref() {
        Some(section) if stamps_match(&section.watched) => section.result.clone(),
        _ => {
            let artifacts = discover::discover_tasks_from_root_artifacts(root.to_path_buf())?;
            let result = artifacts.result.clone();
            cache.discovery = Some(CachedDiscoverySection {
                result: artifacts.result,
                watched: stamps_for_paths(&artifacts.watched_paths),
            });
            cache_dirty = true;
            result
        }
    };

    let ai_tasks = if include_ai_tasks {
        match cache.ai_tasks.as_ref() {
            Some(section) if stamps_match(&section.watched) => section.tasks.clone(),
            _ => {
                let artifacts = ai_tasks::discover_tasks_from_root_artifacts(root.to_path_buf())?;
                let tasks = artifacts.tasks.clone();
                cache.ai_tasks = Some(CachedAiTasksSection {
                    tasks: artifacts.tasks,
                    watched: stamps_for_paths(&artifacts.watched_paths),
                });
                cache_dirty = true;
                tasks
            }
        }
    } else {
        Vec::new()
    };

    if cache_dirty && let Err(err) = write_cache_entry(&cache_path, &cache) {
        tracing::debug!(path = %cache_path.display(), error = %err, "failed to write project snapshot cache");
    }

    Ok((discovery, ai_tasks))
}

fn load_or_build_ai_tasks(root: &Path) -> Result<Vec<ai_tasks::DiscoveredAiTask>> {
    if cache_disabled() {
        return ai_tasks::discover_tasks_from_root(root.to_path_buf());
    }

    let cache_path = snapshot_cache_path(root);
    let mut cache = read_cache_entry(&cache_path).unwrap_or_default();
    if let Some(section) = cache.ai_tasks.as_ref()
        && stamps_match(&section.watched)
    {
        return Ok(section.tasks.clone());
    }

    let artifacts = ai_tasks::discover_tasks_from_root_artifacts(root.to_path_buf())?;
    let tasks = artifacts.tasks.clone();
    cache.ai_tasks = Some(CachedAiTasksSection {
        tasks: artifacts.tasks,
        watched: stamps_for_paths(&artifacts.watched_paths),
    });
    if let Err(err) = write_cache_entry(&cache_path, &cache) {
        tracing::debug!(path = %cache_path.display(), error = %err, "failed to write AI task snapshot cache");
    }
    Ok(tasks)
}

fn cache_disabled() -> bool {
    matches!(
        std::env::var(SNAPSHOT_CACHE_ENV_DISABLE)
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn snapshot_cache_path(root: &Path) -> PathBuf {
    let hash = blake3::hash(root.to_string_lossy().as_bytes()).to_hex();
    crate::config::global_state_dir()
        .join("project-snapshot-cache")
        .join(format!("{hash}.msgpack"))
}

fn read_cache_entry(path: &Path) -> Option<SnapshotCacheEntry> {
    let bytes = fs::read(path).ok()?;
    let cache = rmp_serde::from_slice::<SnapshotCacheEntry>(&bytes).ok()?;
    if cache.version != SNAPSHOT_CACHE_VERSION {
        return None;
    }
    Some(cache)
}

fn write_cache_entry(path: &Path, cache: &SnapshotCacheEntry) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create snapshot cache dir {}", parent.display()))?;
    }

    let mut cache = cache.clone();
    cache.version = SNAPSHOT_CACHE_VERSION;
    let bytes = rmp_serde::to_vec(&cache).context("failed to encode snapshot cache")?;
    let tmp_path = path.with_extension(format!("msgpack.tmp.{}", std::process::id()));
    fs::write(&tmp_path, bytes)
        .with_context(|| format!("failed to write snapshot cache {}", tmp_path.display()))?;
    if let Err(err) = fs::rename(&tmp_path, path) {
        if path.exists() {
            let _ = fs::remove_file(path);
            fs::rename(&tmp_path, path)
                .with_context(|| format!("failed to finalize snapshot cache {}", path.display()))?;
        } else {
            return Err(err)
                .with_context(|| format!("failed to finalize snapshot cache {}", path.display()));
        }
    }
    Ok(())
}

fn stamps_for_paths(paths: &[PathBuf]) -> Vec<PathStamp> {
    let mut stamps: Vec<PathStamp> = paths
        .iter()
        .filter_map(|path| PathStamp::capture(path))
        .collect();
    stamps.sort_by(|a, b| a.path.cmp(&b.path));
    stamps.dedup_by(|a, b| a.path == b.path);
    stamps
}

fn stamps_match(stamps: &[PathStamp]) -> bool {
    stamps.iter().all(PathStamp::matches_current)
}

impl PathStamp {
    fn capture(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            is_dir: metadata.is_dir(),
            len: if metadata.is_file() {
                metadata.len()
            } else {
                0
            },
            modified_sec: modified.as_secs(),
            modified_nsec: modified.subsec_nanos(),
        })
    }

    fn matches_current(&self) -> bool {
        let Some(current) = Self::capture(&self.path) else {
            return false;
        };
        current.is_dir == self.is_dir
            && current.len == self.len
            && current.modified_sec == self.modified_sec
            && current.modified_nsec == self.modified_nsec
    }
}

pub fn canonicalize_root(root: &Path) -> Result<PathBuf> {
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    Ok(root.canonicalize().unwrap_or(root))
}

pub fn resolve_project_root_from_current_dir(climb_to_flow_toml: bool) -> Result<PathBuf> {
    let root = std::env::current_dir()?;
    resolve_project_root_from_start(root, climb_to_flow_toml)
}

pub fn resolve_project_root_from_config(
    config: &Path,
    climb_to_default_flow_toml: bool,
) -> Result<PathBuf> {
    let resolved_config = if config.is_absolute() {
        config.to_path_buf()
    } else {
        std::env::current_dir()?.join(config)
    };
    let root = resolved_config
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let climb = climb_to_default_flow_toml && is_default_flow_config(config);
    resolve_project_root_from_start(root, climb)
}

pub fn is_default_flow_config(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("flow.toml")
}

pub fn find_flow_toml_upwards(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let candidate = current.join("flow.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn resolve_project_root_from_start(start: PathBuf, climb_to_flow_toml: bool) -> Result<PathBuf> {
    let root = if climb_to_flow_toml && !start.join("flow.toml").exists() {
        find_flow_toml_upwards(&start)
            .and_then(|found| found.parent().map(|p| p.to_path_buf()))
            .unwrap_or(start)
    } else {
        start
    };
    Ok(root.canonicalize().unwrap_or(root))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, thread, time::Duration};

    use tempfile::tempdir;

    use super::{PathStamp, find_flow_toml_upwards, resolve_project_root_from_config};

    struct CurrentDirGuard(std::path::PathBuf);

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    #[test]
    fn find_flow_toml_upwards_finds_nearest_ancestor() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        let nested = root.join("a/b/c");
        fs::create_dir_all(&nested).expect("nested dir");
        fs::write(root.join("flow.toml"), "version = 1\nname = \"t\"\n").expect("flow.toml");

        let found = find_flow_toml_upwards(&nested).expect("should find ancestor flow.toml");
        assert_eq!(found, root.join("flow.toml"));
    }

    #[test]
    fn resolve_project_root_from_absolute_config_uses_parent() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).expect("repo dir");
        let config = root.join("flow.toml");
        fs::write(&config, "version = 1\nname = \"t\"\n").expect("flow.toml");

        let resolved =
            resolve_project_root_from_config(&config, true).expect("absolute config resolves");
        assert_eq!(
            resolved,
            root.canonicalize().unwrap_or(root.clone()),
            "absolute config should resolve to its parent"
        );
    }

    #[test]
    fn resolve_project_root_from_relative_config_uses_relative_parent() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("nested dir");
        fs::write(nested.join("flow.toml"), "version = 1\nname = \"t\"\n").expect("flow.toml");

        let previous = std::env::current_dir().expect("current dir");
        let _guard = CurrentDirGuard(previous);
        std::env::set_current_dir(&root).expect("set current dir");
        let resolved = resolve_project_root_from_config(Path::new("nested/flow.toml"), false)
            .expect("relative config resolves");

        assert_eq!(
            resolved,
            nested.canonicalize().unwrap_or(nested.clone()),
            "relative config should resolve to its file parent"
        );
    }

    #[test]
    fn path_stamp_detects_file_changes() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("flow.toml");
        fs::write(&path, "a = 1\n").expect("write file");
        let stamp = PathStamp::capture(&path).expect("capture stamp");

        thread::sleep(Duration::from_millis(5));
        fs::write(&path, "a = 11\n").expect("rewrite file");

        assert!(
            !stamp.matches_current(),
            "file content changes should invalidate the cache stamp"
        );
    }
}
