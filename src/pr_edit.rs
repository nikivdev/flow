use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{RwLock, mpsc};

const STATUS_FILENAME: &str = "status.json";
const INDEX_FILENAME: &str = ".index.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrMeta {
    pub repo: String, // "owner/repo"
    pub pr: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    Clean,
    Dirty,
    Syncing,
    Error,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStatus {
    pub path: String,
    #[serde(default)]
    pub meta: Option<PrMeta>,
    pub state: SyncState,
    #[serde(default)]
    pub last_synced_at_ms: Option<i64>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StatusSnapshot {
    pub updated_at_ms: i64,
    pub files: Vec<FileStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct IndexFile {
    version: u32,
    files: HashMap<String, PrMeta>,
}

#[derive(Clone)]
pub struct PrEditService {
    dir: PathBuf,
    statuses: Arc<RwLock<HashMap<PathBuf, FileStatusInternal>>>,
    index: Arc<RwLock<IndexFile>>,
    gh_token: Arc<RwLock<Option<String>>>,
    client: reqwest::Client,
}

#[derive(Debug, Clone)]
struct FileStatusInternal {
    public: FileStatus,
    last_digest_hex: Option<String>, // sha256(title + "\n" + body)
}

impl PrEditService {
    pub async fn start() -> Result<Arc<Self>> {
        let debug = std::env::var_os("FLOW_PR_EDIT_DEBUG").is_some();
        let dbg = |msg: &str| {
            if debug {
                eprintln!("[pr-edit] {msg}");
            }
        };

        dbg("start");
        let dir = pr_edit_dir()?;
        std::fs::create_dir_all(&dir)?;

        dbg("load index");
        let index = load_index(&dir).unwrap_or_default();
        let svc = Arc::new(Self {
            dir,
            statuses: Arc::new(RwLock::new(HashMap::new())),
            index: Arc::new(RwLock::new(index)),
            gh_token: Arc::new(RwLock::new(None)),
            client: reqwest::Client::builder()
                .user_agent("flow-pr-edit")
                .timeout(Duration::from_secs(20))
                .build()
                .context("failed to build GitHub HTTP client")?,
        });

        // Initial scan so status.json exists and the dashboard has something to show.
        dbg("rescan");
        svc.rescan().await?;

        // Ensure status.json exists even if directory is empty.
        dbg("write status.json");
        let _ = svc.write_status_json().await;

        // Start watcher thread -> tokio event channel.
        dbg("spawn watcher thread");
        let (tx, rx) = mpsc::channel::<PathBuf>(256);
        spawn_watcher_thread(svc.dir.clone(), tx)?;

        // Manager loop: debounce + sync.
        dbg("spawn manager loop");
        let svc_clone = Arc::clone(&svc);
        tokio::spawn(async move {
            if let Err(err) = svc_clone.run_loop(rx).await {
                tracing::warn!(?err, "pr-edit watcher loop exited");
            }
        });

        dbg("ready");
        Ok(svc)
    }

    pub async fn status_snapshot(&self) -> StatusSnapshot {
        let map = self.statuses.read().await;
        let mut files: Vec<FileStatus> = map.values().map(|s| s.public.clone()).collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        StatusSnapshot {
            updated_at_ms: now_ms(),
            files,
        }
    }

    pub async fn rescan(&self) -> Result<()> {
        let dir = self.dir.clone();
        let files = list_md_files(&dir)?;
        let idx = self.index.read().await.clone();

        let mut scanned: Vec<(PathBuf, String, Option<PrMeta>)> = Vec::with_capacity(files.len());
        for path in files {
            let key = path.to_string_lossy().to_string();
            let meta = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| parse_frontmatter(&t))
                .or_else(|| idx.files.get(&key).cloned());
            scanned.push((path, key, meta));
        }

        let mut statuses = self.statuses.write().await;
        for (path, key, meta) in scanned {
            let entry = statuses.entry(path).or_insert_with(|| FileStatusInternal {
                public: FileStatus {
                    path: key,
                    meta: meta.clone(),
                    state: SyncState::Unknown,
                    last_synced_at_ms: None,
                    last_error: None,
                },
                last_digest_hex: None,
            });

            entry.public.meta = meta;
            // Don't auto-sync on startup; just show whether mapping exists.
            entry.public.state = if entry.public.meta.is_some() {
                SyncState::Clean
            } else {
                SyncState::Unknown
            };
            // Preserve last_synced_at_ms / last_error / last_digest_hex from previous runtime.
        }
        drop(statuses);
        self.write_status_json().await?;
        Ok(())
    }

    async fn run_loop(self: Arc<Self>, mut rx: mpsc::Receiver<PathBuf>) -> Result<()> {
        let debounce = Duration::from_millis(1250);
        let mut pending: HashMap<PathBuf, tokio::time::Instant> = HashMap::new();

        loop {
            let next_deadline = pending.values().min().copied();
            tokio::select! {
                maybe_path = rx.recv() => {
                    let Some(path) = maybe_path else { break; };
                    if path.file_name().and_then(|n| n.to_str()) == Some(INDEX_FILENAME) {
                        let _ = self.reload_index_from_disk().await;
                        // Refresh status snapshot so newly mapped files show meta.
                        let _ = self.write_status_json().await;
                        continue;
                    }
                    if should_ignore_event_path(&self.dir, &path) {
                        continue;
                    }
                    pending.insert(path, tokio::time::Instant::now() + debounce);
                }
                _ = async {
                    if let Some(t) = next_deadline {
                        tokio::time::sleep_until(t).await;
                    } else {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                } => {
                    let now = tokio::time::Instant::now();
                    let due: Vec<PathBuf> = pending
                        .iter()
                        .filter_map(|(p, t)| if *t <= now { Some(p.clone()) } else { None })
                        .collect();
                    if due.is_empty() {
                        continue;
                    }
                    for p in &due {
                        pending.remove(p);
                    }

                    let mut any_changed = false;
                    for path in due {
                        if let Err(err) = self.sync_file(&path).await {
                            tracing::debug!(?err, path=%path.display(), "pr-edit sync failed");
                        }
                        any_changed = true;
                    }
                    if any_changed {
                        let _ = self.write_status_json().await;
                    }
                }
            }
        }

        Ok(())
    }

    async fn sync_file(&self, path: &Path) -> Result<()> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return Ok(()), // deleted/unreadable
        };

        // Resolve PR identity.
        let fm = parse_frontmatter(&text);
        let idx = if fm.is_none() {
            self.lookup_index(path).await
        } else {
            None
        };
        let meta = fm.or(idx);

        // Parse PR title/body from markdown.
        let (title, body) = match parse_title_body(&text) {
            Ok(v) => v,
            Err(err) => {
                self.set_error(path, meta, format!("{err:#}")).await;
                return Ok(());
            }
        };

        let digest_hex = compute_digest_hex(&title, &body);

        {
            let mut statuses = self.statuses.write().await;
            let entry = statuses
                .entry(path.to_path_buf())
                .or_insert_with(|| FileStatusInternal {
                    public: FileStatus {
                        path: path.to_string_lossy().to_string(),
                        meta: meta.clone(),
                        state: SyncState::Unknown,
                        last_synced_at_ms: None,
                        last_error: None,
                    },
                    last_digest_hex: None,
                });
            entry.public.meta = meta.clone();

            if entry.last_digest_hex.as_deref() == Some(&digest_hex)
                && entry.public.last_error.is_none()
            {
                entry.public.state = SyncState::Clean;
                return Ok(());
            }

            entry.public.state = SyncState::Syncing;
            entry.public.last_error = None;
        }

        let Some(meta) = meta else {
            self.set_error(
                path,
                None,
                "missing PR metadata (add YAML frontmatter with repo/pr)".to_string(),
            )
            .await;
            return Ok(());
        };

        // Ensure token exists (cached).
        let token = match self.get_gh_token().await {
            Ok(t) => t,
            Err(err) => {
                self.set_error(path, Some(meta), format!("{err:#}")).await;
                return Ok(());
            }
        };

        // PATCH the PR issue (PRs are issues too).
        let url = format!(
            "https://api.github.com/repos/{}/issues/{}",
            meta.repo, meta.pr
        );
        let resp = match self
            .client
            .patch(url)
            .bearer_auth(token)
            .json(&serde_json::json!({ "title": title, "body": body }))
            .send()
            .await
        {
            Ok(r) => r,
            Err(err) => {
                self.set_error(
                    path,
                    Some(meta),
                    format!("GitHub PATCH request failed: {err:#}"),
                )
                .await;
                return Ok(());
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            self.set_error(
                path,
                Some(meta),
                format!("GitHub API error {status}: {body_text}"),
            )
            .await;
            return Ok(());
        }

        let mut statuses = self.statuses.write().await;
        let entry = statuses
            .entry(path.to_path_buf())
            .or_insert_with(|| FileStatusInternal {
                public: FileStatus {
                    path: path.to_string_lossy().to_string(),
                    meta: Some(meta.clone()),
                    state: SyncState::Unknown,
                    last_synced_at_ms: None,
                    last_error: None,
                },
                last_digest_hex: None,
            });
        entry.public.meta = Some(meta);
        entry.public.state = SyncState::Clean;
        entry.public.last_synced_at_ms = Some(now_ms());
        entry.public.last_error = None;
        entry.last_digest_hex = Some(digest_hex);

        Ok(())
    }

    async fn lookup_index(&self, path: &Path) -> Option<PrMeta> {
        let key = path.to_string_lossy().to_string();
        let guard = self.index.read().await;
        guard.files.get(&key).cloned()
    }

    async fn set_error(&self, path: &Path, meta: Option<PrMeta>, err: String) {
        let mut statuses = self.statuses.write().await;
        let entry = statuses
            .entry(path.to_path_buf())
            .or_insert_with(|| FileStatusInternal {
                public: FileStatus {
                    path: path.to_string_lossy().to_string(),
                    meta: meta.clone(),
                    state: SyncState::Error,
                    last_synced_at_ms: None,
                    last_error: Some(err.clone()),
                },
                last_digest_hex: None,
            });
        entry.public.meta = meta;
        entry.public.state = SyncState::Error;
        entry.public.last_error = Some(err);
    }

    async fn get_gh_token(&self) -> Result<String> {
        if let Some(t) = self.gh_token.read().await.clone() {
            return Ok(t);
        }

        let out = Command::new("gh")
            .args(["auth", "token"])
            .output()
            .context("failed to run `gh auth token`")?;
        if !out.status.success() {
            bail!("`gh auth token` failed; run `gh auth login`");
        }
        let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if token.is_empty() {
            bail!("`gh auth token` returned empty token");
        }

        *self.gh_token.write().await = Some(token.clone());
        Ok(token)
    }

    async fn write_status_json(&self) -> Result<()> {
        let snapshot = self.status_snapshot().await;
        let json = serde_json::to_string_pretty(&snapshot)?;

        let tmp = self.dir.join(format!(".{STATUS_FILENAME}.tmp"));
        let out = self.dir.join(STATUS_FILENAME);
        std::fs::write(&tmp, json)?;
        // Best-effort atomic replace.
        let _ = std::fs::rename(&tmp, &out);
        Ok(())
    }

    pub fn pr_edit_dir_path(&self) -> &Path {
        &self.dir
    }

    async fn reload_index_from_disk(&self) -> Result<()> {
        let dir = self.dir.clone();
        let idx = tokio::task::spawn_blocking(move || load_index(&dir))
            .await
            .context("index reload task panicked")??;
        *self.index.write().await = idx;
        Ok(())
    }
}

fn pr_edit_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not resolve home directory")?;
    Ok(home.join(".flow").join("pr-edit"))
}

fn list_md_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?;
    for ent in entries {
        let Ok(ent) = ent else {
            continue;
        };
        let p = ent.path();
        if is_md_file(&p) {
            out.push(p);
        }
    }
    Ok(out)
}

fn is_md_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    if ext != "md" {
        return false;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.starts_with('.') {
        return false;
    }
    if name.ends_with('~') {
        return false;
    }
    true
}

fn should_ignore_event_path(dir: &Path, path: &Path) -> bool {
    // Ignore non-files and non-md updates.
    if path == dir.join(STATUS_FILENAME) {
        return true;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name == STATUS_FILENAME {
        return true;
    }
    if name.starts_with(".")
        || name.ends_with("~")
        || name.ends_with(".swp")
        || name.ends_with(".tmp")
    {
        return true;
    }
    if !name.ends_with(".md") {
        return true;
    }
    false
}

fn spawn_watcher_thread(dir: PathBuf, tx: mpsc::Sender<PathBuf>) -> Result<()> {
    std::thread::spawn(move || {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let mut debouncer = match new_debouncer(Duration::from_millis(250), event_tx) {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(?err, "failed to init pr-edit watcher");
                return;
            }
        };
        if let Err(err) = debouncer.watcher().watch(&dir, RecursiveMode::NonRecursive) {
            tracing::warn!(?err, dir=%dir.display(), "failed to watch pr-edit directory");
            return;
        }

        loop {
            match event_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(events)) => {
                    for e in events {
                        let p = e.path;
                        let is_md = p.extension().and_then(|x| x.to_str()) == Some("md");
                        let is_index =
                            p.file_name().and_then(|n| n.to_str()) == Some(INDEX_FILENAME);
                        // Only enqueue md files + index updates; manager will do more filtering.
                        if is_md || is_index {
                            let _ = tx.blocking_send(p);
                        }
                    }
                }
                Ok(Err(err)) => {
                    tracing::debug!(?err, "pr-edit watcher error");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    Ok(())
}

fn parse_frontmatter(text: &str) -> Option<PrMeta> {
    let mut lines = text.lines();
    let first = lines.next()?.trim();
    if first != "---" {
        return None;
    }

    let mut repo: Option<String> = None;
    let mut pr: Option<u64> = None;
    for line in lines {
        let l = line.trim();
        if l == "---" {
            break;
        }
        if let Some(v) = l.strip_prefix("repo:") {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                repo = Some(v.to_string());
            }
        }
        if let Some(v) = l.strip_prefix("pr:") {
            let v = v.trim();
            if let Ok(n) = v.parse::<u64>() {
                pr = Some(n);
            }
        }
    }

    match (repo, pr) {
        (Some(repo), Some(pr)) => Some(PrMeta { repo, pr }),
        _ => None,
    }
}

fn parse_title_body(text: &str) -> Result<(String, String)> {
    let mut title: Option<String> = None;
    let mut body_lines: Vec<String> = Vec::new();

    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let l = line.trim_end();
        if l.trim() == "# Title" {
            while let Some(nl) = lines.peek() {
                if nl.trim().is_empty() {
                    lines.next();
                } else {
                    break;
                }
            }
            if let Some(nl) = lines.peek() {
                let t = nl.trim();
                if !t.is_empty() {
                    title = Some(t.to_string());
                }
            }
            continue;
        }
        if l.trim() == "# Description" {
            while let Some(nl) = lines.peek() {
                if nl.trim().is_empty() {
                    lines.next();
                } else {
                    break;
                }
            }
            for rest in lines {
                body_lines.push(rest.to_string());
            }
            break;
        }
    }

    let title = title.unwrap_or_default().trim().to_string();
    if title.is_empty() {
        bail!("missing PR title (expected a non-empty line under `# Title`)");
    }
    let body = body_lines.join("\n").trim_end().to_string();
    Ok((title, body))
}

fn write_index(dir: &Path, idx: &IndexFile) -> Result<()> {
    let path = dir.join(INDEX_FILENAME);
    let json = serde_json::to_string_pretty(idx)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Best-effort helper for other codepaths (e.g. `f pr open edit`) to register mappings for files
/// that don't (yet) have frontmatter.
pub fn index_upsert_file(path: &Path, repo: &str, pr: u64) -> Result<()> {
    let dir = pr_edit_dir()?;
    std::fs::create_dir_all(&dir)?;
    let mut idx = load_index(&dir).unwrap_or_default();
    idx.files.insert(
        path.to_string_lossy().to_string(),
        PrMeta {
            repo: repo.to_string(),
            pr,
        },
    );
    write_index(&dir, &idx)
}

fn compute_digest_hex(title: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(title.as_bytes());
    hasher.update(b"\n");
    hasher.update(body.as_bytes());
    hex::encode(hasher.finalize())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as i64
}

fn load_index(dir: &Path) -> Result<IndexFile> {
    let path = dir.join(INDEX_FILENAME);
    if !path.exists() {
        return Ok(IndexFile {
            version: 1,
            files: HashMap::new(),
        });
    }
    let text = std::fs::read_to_string(&path)?;
    let mut parsed: IndexFile = serde_json::from_str(&text)?;
    if parsed.version == 0 {
        parsed.version = 1;
    }
    Ok(parsed)
}
