use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{config, project_snapshot};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const CACHE_VERSION: u32 = 1;
const RECENT_REFRESH_LIMIT: usize = 12;
const AI_MANIFEST_CACHE_ENV_DISABLE: &str = "FLOW_DISABLE_AI_PROJECT_MANIFEST_CACHE";
const IGNORED_LOCAL_BUCKETS: &[&str] = &[
    "reviews",
    "test",
    "tmp",
    "cache",
    "artifacts",
    "traces",
    "generated",
    "scratch",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AiReviewPacketRef {
    pub markdown_path: String,
    pub json_path: Option<String>,
    pub updated_at_unix: u64,
    pub pr_number: Option<u64>,
    pub repo_slug: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AiProjectManifest {
    pub repo_root: String,
    pub generated_at: String,
    pub generated_at_unix: u64,
    pub has_ai_dir: bool,
    pub has_context: bool,
    pub has_skills: bool,
    pub has_docs: bool,
    pub has_reviews: bool,
    pub has_tasks: bool,
    pub has_todos: bool,
    pub has_repos_toml: bool,
    pub skills_count: usize,
    pub docs_count: usize,
    pub reviews_count: usize,
    pub tasks_count: usize,
    pub todos_count: usize,
    pub open_todos_count: usize,
    pub latest_review_packet: Option<AiReviewPacketRef>,
    pub latest_context_doc: Option<String>,
    pub latest_task_paths: Vec<String>,
    pub latest_skill_names: Vec<String>,
    pub ignored_local_buckets_present: Vec<String>,
    pub query_count: u64,
    pub last_requested_at_unix: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedManifestEntry {
    version: u32,
    manifest: AiProjectManifest,
    watched: Vec<PathStamp>,
}

#[derive(Debug, Clone)]
struct MemoryCacheEntry {
    manifest: AiProjectManifest,
    watched: Vec<PathStamp>,
}

#[derive(Debug, Default)]
struct MemoryCache {
    entries: HashMap<PathBuf, MemoryCacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PathStamp {
    path: PathBuf,
    is_dir: bool,
    len: u64,
    modified_sec: u64,
    modified_nsec: u32,
}

#[derive(Debug, Deserialize)]
struct TodoItem {
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReviewPacketSummary {
    repo: Option<String>,
    pr_number: Option<u64>,
}

fn memory_cache() -> &'static Mutex<MemoryCache> {
    static CACHE: OnceLock<Mutex<MemoryCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(MemoryCache::default()))
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

fn cache_disabled() -> bool {
    matches!(
        std::env::var(AI_MANIFEST_CACHE_ENV_DISABLE)
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn cache_dir() -> PathBuf {
    config::global_state_dir()
        .join("codex")
        .join("project-ai-manifest")
}

fn cache_path_for_repo_root(repo_root: &Path) -> PathBuf {
    let hash = blake3::hash(repo_root.to_string_lossy().as_bytes()).to_hex();
    cache_dir().join(format!("{hash}.msgpack"))
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

fn stamps_match(stamps: &[PathStamp]) -> bool {
    stamps.iter().all(PathStamp::matches_current)
}

fn write_cached_manifest(path: &Path, entry: &CachedManifestEntry) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let bytes = rmp_serde::to_vec(entry).context("failed to encode ai project manifest cache")?;
    let tmp_path = path.with_extension(format!("msgpack.tmp.{}", std::process::id()));
    fs::write(&tmp_path, bytes)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    if let Err(err) = fs::rename(&tmp_path, path) {
        if path.exists() {
            let _ = fs::remove_file(path);
            fs::rename(&tmp_path, path)
                .with_context(|| format!("failed to finalize {}", path.display()))?;
        } else {
            return Err(err).with_context(|| format!("failed to finalize {}", path.display()));
        }
    }
    Ok(())
}

fn read_cached_manifest(path: &Path) -> Option<CachedManifestEntry> {
    let bytes = fs::read(path).ok()?;
    let entry = rmp_serde::from_slice::<CachedManifestEntry>(&bytes).ok()?;
    if entry.version != CACHE_VERSION {
        return None;
    }
    Some(entry)
}

fn resolve_repo_root(target_path: &Path) -> Result<PathBuf> {
    let canonical = project_snapshot::canonicalize_root(target_path)?;
    if let Some(root) = detect_git_root(&canonical) {
        return Ok(root);
    }
    if let Some(flow_toml) = project_snapshot::find_flow_toml_upwards(&canonical)
        && let Some(parent) = flow_toml.parent()
    {
        return Ok(parent.to_path_buf());
    }
    if canonical.is_dir() {
        return Ok(canonical);
    }
    Ok(canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| canonical.clone()))
}

fn detect_git_root(path: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .current_dir(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn make_display_path(path: &Path) -> String {
    path.display().to_string()
}

fn format_unix_secs(secs: u64) -> String {
    let dt: DateTime<Utc> =
        DateTime::<Utc>::from(UNIX_EPOCH + std::time::Duration::from_secs(secs));
    dt.to_rfc3339()
}

fn read_dir_paths(path: &Path) -> Vec<PathBuf> {
    let mut entries = fs::read_dir(path)
        .ok()
        .into_iter()
        .flat_map(|iter| iter.filter_map(|entry| entry.ok().map(|item| item.path())))
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn collect_markdown_files(path: &Path) -> Vec<PathBuf> {
    read_dir_paths(path)
        .into_iter()
        .filter(|entry| {
            entry.is_file() && entry.extension().and_then(|ext| ext.to_str()) == Some("md")
        })
        .collect()
}

fn collect_skill_markers(path: &Path) -> Vec<(String, PathBuf)> {
    let mut skills = read_dir_paths(path)
        .into_iter()
        .filter(|entry| entry.is_dir())
        .filter_map(|entry| {
            let marker = entry.join("SKILL.md");
            if marker.is_file() {
                Some((
                    entry
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default()
                        .to_string(),
                    marker,
                ))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    skills.sort_by(|a, b| a.0.cmp(&b.0));
    skills
}

fn collect_task_files(root: &Path, results: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_task_files(&path, results);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("mbt") {
            results.push(path);
        }
    }
}

fn collect_review_packet_files(path: &Path) -> Vec<PathBuf> {
    let mut files = read_dir_paths(path)
        .into_iter()
        .filter(|entry| entry.is_file())
        .filter(|entry| {
            let Some(name) = entry.file_name().and_then(|value| value.to_str()) else {
                return false;
            };
            name.starts_with("pr-feedback-")
                && matches!(
                    entry.extension().and_then(|value| value.to_str()),
                    Some("md" | "json")
                )
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn file_mtime_unix(path: &Path) -> u64 {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

fn derive_latest_review_packet(review_files: &[PathBuf]) -> Option<AiReviewPacketRef> {
    let mut newest_stem: Option<String> = None;
    let mut newest_updated_at = 0u64;
    let mut stem_paths: HashMap<String, (Option<PathBuf>, Option<PathBuf>, u64)> = HashMap::new();

    for file in review_files {
        let stem = file.file_stem()?.to_string_lossy().to_string();
        let updated_at = file_mtime_unix(file);
        let entry = stem_paths.entry(stem.clone()).or_insert((None, None, 0));
        match file.extension().and_then(|ext| ext.to_str()) {
            Some("md") => entry.0 = Some(file.clone()),
            Some("json") => entry.1 = Some(file.clone()),
            _ => {}
        }
        entry.2 = entry.2.max(updated_at);
        if updated_at >= newest_updated_at {
            newest_updated_at = updated_at;
            newest_stem = Some(stem);
        }
    }

    let stem = newest_stem?;
    let (markdown_path, json_path, updated_at_unix) = stem_paths.remove(&stem)?;
    let mut repo_slug = None;
    let mut pr_number = None;
    if let Some(json_path_ref) = json_path.as_ref()
        && let Ok(content) = fs::read_to_string(json_path_ref)
        && let Ok(summary) = serde_json::from_str::<ReviewPacketSummary>(&content)
    {
        repo_slug = summary.repo;
        pr_number = summary.pr_number;
    }

    Some(AiReviewPacketRef {
        markdown_path: markdown_path
            .as_ref()
            .map(|path| make_display_path(path))
            .unwrap_or_default(),
        json_path: json_path.as_ref().map(|path| make_display_path(path)),
        updated_at_unix,
        pr_number,
        repo_slug,
    })
}

fn read_todo_counts(path: &Path) -> (usize, usize) {
    let Ok(content) = fs::read_to_string(path) else {
        return (0, 0);
    };
    let Ok(items) = serde_json::from_str::<Vec<TodoItem>>(&content) else {
        return (0, 0);
    };
    let open = items
        .iter()
        .filter(|item| item.status.as_deref() == Some("pending"))
        .count();
    (items.len(), open)
}

fn build_manifest(repo_root: &Path) -> Result<(AiProjectManifest, Vec<PathStamp>)> {
    let repo_root = project_snapshot::canonicalize_root(repo_root)?;
    let ai_dir = repo_root.join(".ai");
    let has_ai_dir = ai_dir.is_dir();
    let now = unix_now_secs();
    let mut watched = Vec::new();
    let mut latest_context_doc = None;
    let mut latest_review_packet = None;
    let mut latest_task_paths = Vec::new();
    let mut latest_skill_names = Vec::new();
    let mut ignored_local_buckets_present = Vec::new();
    let mut skills_count = 0usize;
    let mut docs_count = 0usize;
    let mut reviews_count = 0usize;
    let mut tasks_count = 0usize;
    let mut todos_count = 0usize;
    let mut open_todos_count = 0usize;
    let mut has_context = false;
    let mut has_skills = false;
    let mut has_docs = false;
    let mut has_reviews = false;
    let mut has_tasks = false;
    let mut has_todos = false;
    let has_repos_toml = ai_dir.join("repos.toml").is_file();

    if has_ai_dir {
        if let Some(stamp) = PathStamp::capture(&ai_dir) {
            watched.push(stamp);
        }

        let context_dir = ai_dir.join("context");
        if context_dir.is_dir() {
            has_context = true;
            let context_docs = collect_markdown_files(&context_dir);
            if let Some(path) = context_docs.iter().max_by_key(|path| file_mtime_unix(path)) {
                latest_context_doc = Some(make_display_path(path));
            }
            watched.extend(
                context_docs
                    .iter()
                    .filter_map(|path| PathStamp::capture(path)),
            );
            if let Some(stamp) = PathStamp::capture(&context_dir) {
                watched.push(stamp);
            }
        }

        let docs_dir = ai_dir.join("docs");
        if docs_dir.is_dir() {
            let docs = collect_markdown_files(&docs_dir);
            docs_count = docs.len();
            has_docs = docs_count > 0;
            watched.extend(docs.iter().filter_map(|path| PathStamp::capture(path)));
            if let Some(stamp) = PathStamp::capture(&docs_dir) {
                watched.push(stamp);
            }
        }

        let skills_dir = ai_dir.join("skills");
        if skills_dir.is_dir() {
            let skills = collect_skill_markers(&skills_dir);
            skills_count = skills.len();
            has_skills = skills_count > 0;
            latest_skill_names = skills
                .iter()
                .map(|(name, _)| name.clone())
                .take(16)
                .collect();
            watched.extend(
                skills
                    .iter()
                    .filter_map(|(_, path)| PathStamp::capture(path)),
            );
            if let Some(stamp) = PathStamp::capture(&skills_dir) {
                watched.push(stamp);
            }
        }

        let reviews_dir = ai_dir.join("reviews");
        if reviews_dir.is_dir() {
            let review_files = collect_review_packet_files(&reviews_dir);
            reviews_count = review_files.len();
            has_reviews = reviews_count > 0;
            latest_review_packet = derive_latest_review_packet(&review_files);
            watched.extend(
                review_files
                    .iter()
                    .filter_map(|path| PathStamp::capture(path)),
            );
            if let Some(stamp) = PathStamp::capture(&reviews_dir) {
                watched.push(stamp);
            }
        }

        let tasks_dir = ai_dir.join("tasks");
        if tasks_dir.is_dir() {
            let mut task_files = Vec::new();
            collect_task_files(&tasks_dir, &mut task_files);
            task_files.sort_by_key(|path| file_mtime_unix(path));
            tasks_count = task_files.len();
            has_tasks = tasks_count > 0;
            latest_task_paths = task_files
                .iter()
                .rev()
                .take(8)
                .map(|path| make_display_path(path))
                .collect();
            watched.extend(
                task_files
                    .iter()
                    .filter_map(|path| PathStamp::capture(path)),
            );
            if let Some(stamp) = PathStamp::capture(&tasks_dir) {
                watched.push(stamp);
            }
        }

        let todos_path = ai_dir.join("todos").join("todos.json");
        if todos_path.is_file() {
            has_todos = true;
            (todos_count, open_todos_count) = read_todo_counts(&todos_path);
            if let Some(stamp) = PathStamp::capture(&todos_path) {
                watched.push(stamp);
            }
        }

        if has_repos_toml && let Some(stamp) = PathStamp::capture(&ai_dir.join("repos.toml")) {
            watched.push(stamp);
        }

        for bucket in IGNORED_LOCAL_BUCKETS {
            let path = ai_dir.join(bucket);
            if path.exists() {
                ignored_local_buckets_present.push((*bucket).to_string());
                if let Some(stamp) = PathStamp::capture(&path) {
                    watched.push(stamp);
                }
            }
        }
    }

    watched.sort_by(|a, b| a.path.cmp(&b.path));
    watched.dedup_by(|a, b| a.path == b.path);

    Ok((
        AiProjectManifest {
            repo_root: make_display_path(&repo_root),
            generated_at: format_unix_secs(now),
            generated_at_unix: now,
            has_ai_dir,
            has_context,
            has_skills,
            has_docs,
            has_reviews,
            has_tasks,
            has_todos,
            has_repos_toml,
            skills_count,
            docs_count,
            reviews_count,
            tasks_count,
            todos_count,
            open_todos_count,
            latest_review_packet,
            latest_context_doc,
            latest_task_paths,
            latest_skill_names,
            ignored_local_buckets_present,
            query_count: 0,
            last_requested_at_unix: None,
        },
        watched,
    ))
}

fn persist_cache_entry(repo_root: &Path, manifest: &AiProjectManifest, watched: &[PathStamp]) {
    if cache_disabled() {
        return;
    }
    let path = cache_path_for_repo_root(repo_root);
    let entry = CachedManifestEntry {
        version: CACHE_VERSION,
        manifest: manifest.clone(),
        watched: watched.to_vec(),
    };
    if let Err(err) = write_cached_manifest(&path, &entry) {
        tracing::debug!(path = %path.display(), error = %err, "failed to write ai project manifest cache");
    }
}

pub fn load_for_target(target_path: &Path, refresh: bool) -> Result<AiProjectManifest> {
    let repo_root = resolve_repo_root(target_path)?;
    load_for_repo_root_with_usage(&repo_root, refresh, true)
}

pub fn load_for_target_without_usage(
    target_path: &Path,
    refresh: bool,
) -> Result<AiProjectManifest> {
    let repo_root = resolve_repo_root(target_path)?;
    load_for_repo_root_with_usage(&repo_root, refresh, false)
}

fn load_for_repo_root_with_usage(
    repo_root: &Path,
    refresh: bool,
    record_usage: bool,
) -> Result<AiProjectManifest> {
    let repo_root = project_snapshot::canonicalize_root(repo_root)?;
    let now = unix_now_secs();

    {
        let mut cache = memory_cache()
            .lock()
            .expect("ai project manifest cache mutex poisoned");
        if let Some(entry) = cache.entries.get_mut(&repo_root)
            && !refresh
            && stamps_match(&entry.watched)
        {
            if record_usage {
                entry.manifest.query_count += 1;
                entry.manifest.last_requested_at_unix = Some(now);
                persist_cache_entry(&repo_root, &entry.manifest, &entry.watched);
            }
            return Ok(entry.manifest.clone());
        }
    }

    let cached_disk = if refresh || cache_disabled() {
        None
    } else {
        read_cached_manifest(&cache_path_for_repo_root(&repo_root))
    };

    if let Some(mut entry) = cached_disk
        && stamps_match(&entry.watched)
    {
        if record_usage {
            entry.manifest.query_count += 1;
            entry.manifest.last_requested_at_unix = Some(now);
        }
        {
            let mut cache = memory_cache()
                .lock()
                .expect("ai project manifest cache mutex poisoned");
            cache.entries.insert(
                repo_root.clone(),
                MemoryCacheEntry {
                    manifest: entry.manifest.clone(),
                    watched: entry.watched.clone(),
                },
            );
        }
        persist_cache_entry(&repo_root, &entry.manifest, &entry.watched);
        return Ok(entry.manifest);
    }

    let previous_usage = memory_cache()
        .lock()
        .expect("ai project manifest cache mutex poisoned")
        .entries
        .get(&repo_root)
        .map(|entry| {
            (
                entry.manifest.query_count,
                entry.manifest.last_requested_at_unix,
            )
        })
        .or_else(|| {
            read_cached_manifest(&cache_path_for_repo_root(&repo_root)).map(|entry| {
                (
                    entry.manifest.query_count,
                    entry.manifest.last_requested_at_unix,
                )
            })
        })
        .unwrap_or((0, None));

    let (mut manifest, watched) = build_manifest(&repo_root)?;
    if record_usage {
        manifest.query_count = previous_usage.0.saturating_add(1);
        manifest.last_requested_at_unix = Some(now.max(previous_usage.1.unwrap_or(0)));
    } else {
        manifest.query_count = previous_usage.0;
        manifest.last_requested_at_unix = previous_usage.1;
    }

    {
        let mut cache = memory_cache()
            .lock()
            .expect("ai project manifest cache mutex poisoned");
        cache.entries.insert(
            repo_root.clone(),
            MemoryCacheEntry {
                manifest: manifest.clone(),
                watched: watched.clone(),
            },
        );
    }
    persist_cache_entry(&repo_root, &manifest, &watched);
    Ok(manifest)
}

pub fn recent(limit: usize) -> Result<Vec<AiProjectManifest>> {
    let limit = limit.clamp(1, 50);
    let mut manifests = Vec::new();
    let dir = cache_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let entries =
        fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("msgpack") {
            continue;
        }
        if let Some(entry) = read_cached_manifest(&path) {
            if !Path::new(&entry.manifest.repo_root).exists() {
                continue;
            }
            manifests.push(entry.manifest);
        }
    }
    manifests.sort_by(|a, b| {
        b.last_requested_at_unix
            .unwrap_or(0)
            .cmp(&a.last_requested_at_unix.unwrap_or(0))
            .then_with(|| b.generated_at_unix.cmp(&a.generated_at_unix))
    });
    manifests.truncate(limit);
    Ok(manifests)
}

pub fn refresh_recent(limit: usize) -> Result<usize> {
    let manifests = recent(limit.max(RECENT_REFRESH_LIMIT))?;
    let mut refreshed = 0usize;
    for manifest in manifests.into_iter().take(limit.max(1)) {
        let repo_root = PathBuf::from(&manifest.repo_root);
        if !repo_root.exists() {
            continue;
        }
        let _ = load_for_repo_root_with_usage(&repo_root, true, false)?;
        refreshed += 1;
    }
    Ok(refreshed)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::{
        build_manifest, cache_path_for_repo_root, load_for_repo_root_with_usage, load_for_target,
    };

    #[test]
    fn build_manifest_counts_bounded_ai_surfaces() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        let ai = root.join(".ai");
        fs::create_dir_all(ai.join("docs")).expect("docs dir");
        fs::create_dir_all(ai.join("skills/foo")).expect("skills dir");
        fs::create_dir_all(ai.join("reviews")).expect("reviews dir");
        fs::create_dir_all(ai.join("tasks/demo")).expect("tasks dir");
        fs::create_dir_all(ai.join("todos")).expect("todos dir");
        fs::create_dir_all(ai.join("cache")).expect("cache dir");
        fs::write(ai.join("docs/one.md"), "# one\n").expect("doc");
        fs::write(
            ai.join("skills/foo/SKILL.md"),
            "---\nname: foo\ndescription: test\n---\n",
        )
        .expect("skill");
        fs::write(
            ai.join("reviews/pr-feedback-42.json"),
            r#"{"repo":"acme/repo","pr_number":42}"#,
        )
        .expect("review json");
        fs::write(ai.join("reviews/pr-feedback-42.md"), "# review\n").expect("review md");
        fs::write(ai.join("tasks/demo/main.mbt"), "// task\n").expect("task");
        fs::write(
            ai.join("todos/todos.json"),
            r#"[{"status":"pending"},{"status":"completed"}]"#,
        )
        .expect("todos");

        let (manifest, watched) = build_manifest(&root).expect("manifest");
        assert!(manifest.has_ai_dir);
        assert!(manifest.has_docs);
        assert!(manifest.has_skills);
        assert!(manifest.has_reviews);
        assert!(manifest.has_tasks);
        assert!(manifest.has_todos);
        assert_eq!(manifest.docs_count, 1);
        assert_eq!(manifest.skills_count, 1);
        assert_eq!(manifest.reviews_count, 2);
        assert_eq!(manifest.tasks_count, 1);
        assert_eq!(manifest.todos_count, 2);
        assert_eq!(manifest.open_todos_count, 1);
        assert!(
            manifest
                .ignored_local_buckets_present
                .contains(&"cache".to_string())
        );
        let packet = manifest.latest_review_packet.expect("review packet");
        assert_eq!(packet.pr_number, Some(42));
        assert_eq!(packet.repo_slug.as_deref(), Some("acme/repo"));
        assert!(!watched.is_empty());
        let _ = fs::remove_file(cache_path_for_repo_root(&root));
    }

    #[test]
    fn load_for_target_tracks_query_count() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join(".ai/docs")).expect("docs dir");
        fs::write(root.join(".ai/docs/one.md"), "# one\n").expect("doc");

        let first = load_for_target(&root, false).expect("first");
        thread::sleep(Duration::from_millis(5));
        let second = load_for_target(&root, false).expect("second");

        assert_eq!(first.query_count, 1);
        assert_eq!(second.query_count, 2);
        assert!(second.last_requested_at_unix.is_some());
        let _ = fs::remove_file(cache_path_for_repo_root(&root));
    }

    #[test]
    fn background_refresh_preserves_usage_counters() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join(".ai/docs")).expect("docs dir");
        fs::write(root.join(".ai/docs/one.md"), "# one\n").expect("doc");

        let first = load_for_target(&root, false).expect("first");
        let refreshed =
            load_for_repo_root_with_usage(&root, true, false).expect("background refresh");

        assert_eq!(first.query_count, 1);
        assert_eq!(refreshed.query_count, 1);
        assert_eq!(
            refreshed.last_requested_at_unix,
            first.last_requested_at_unix
        );
        let _ = fs::remove_file(cache_path_for_repo_root(&root));
    }
}
