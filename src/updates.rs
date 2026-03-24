use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tempfile::NamedTempFile;

use crate::cli::UpdatesCommand;
use crate::{commit::configured_codex_bin_for_workdir, config};

const INDEX_VERSION: u32 = 1;
const DEFAULT_OUTPUT_ROOT: &str = "~/docs/updates";
const MAX_SUBJECTS: usize = 160;
const MAX_REPRESENTATIVE_COMMITS: usize = 8;
const MAX_REPRESENTATIVE_CHARS: usize = 2200;
const MAX_DIFFSTAT_CHARS: usize = 4000;
const MAX_COUNT_LINES: usize = 12;
const MAX_SCOPE_HISTORY: usize = 50;

#[derive(Debug, Clone)]
pub enum RunOutcome {
    Written {
        output_path: PathBuf,
        markdown: String,
        commit_count: usize,
    },
    Printed {
        markdown: String,
        commit_count: usize,
    },
    NoChanges {
        message: String,
    },
}

#[derive(Debug, Clone, Default)]
struct RuntimeOverrides {
    codex_bin: Option<String>,
    output_root: Option<PathBuf>,
    state_root: Option<PathBuf>,
    now: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdatesIndex {
    version: u32,
    repo_root: String,
    repo_slug: String,
    scopes: BTreeMap<String, UpdateScopeIndex>,
}

impl Default for UpdatesIndex {
    fn default() -> Self {
        Self {
            version: INDEX_VERSION,
            repo_root: String::new(),
            repo_slug: String::new(),
            scopes: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct UpdateScopeIndex {
    ref_name: String,
    pathspecs: Vec<String>,
    runs: Vec<UpdateRunRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateRunRecord {
    generated_at: String,
    mode: String,
    from_commit: Option<String>,
    from_label: String,
    to_commit: String,
    commit_count: usize,
    output_path: Option<String>,
}

#[derive(Debug, Clone)]
struct RepoTarget {
    repo_root: PathBuf,
    repo_slug: String,
    remote: Option<String>,
    ref_name: String,
    tip_commit: String,
    pathspecs: Vec<String>,
    scope_key: String,
    scope_state: UpdateScopeIndex,
    index_path: PathBuf,
    index: UpdatesIndex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionMode {
    Incremental,
    InitialLookback,
    ForcedLookback,
    ExplicitSince,
    HistoryRewritten,
}

impl SelectionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Incremental => "incremental",
            Self::InitialLookback => "initial-lookback",
            Self::ForcedLookback => "forced-lookback",
            Self::ExplicitSince => "explicit-since",
            Self::HistoryRewritten => "history-rewritten",
        }
    }
}

#[derive(Debug, Clone)]
struct DeltaSelection {
    mode: SelectionMode,
    from_commit: Option<String>,
    from_label: String,
    commits: Vec<CommitRecord>,
    previous_output_path: Option<String>,
    note: Option<String>,
    diff_base: Option<String>,
}

#[derive(Debug, Clone)]
struct CommitRecord {
    sha: String,
    short_sha: String,
    author: String,
    date: String,
    subject: String,
    files: Vec<String>,
}

impl CommitRecord {
    fn file_count(&self) -> usize {
        self.files.len()
    }

    fn ref_line(&self) -> String {
        format!("{} {}", self.short_sha, self.subject)
    }
}

#[derive(Debug, Clone)]
struct UpdateEvidence {
    generated_at: DateTime<Utc>,
    repo_root: PathBuf,
    repo_slug: String,
    remote: Option<String>,
    ref_name: String,
    tip_commit: String,
    pathspecs: Vec<String>,
    mode: SelectionMode,
    from_commit: Option<String>,
    from_label: String,
    previous_output_path: Option<String>,
    commits: Vec<CommitRecord>,
    oldest_date: Option<String>,
    newest_date: Option<String>,
    author_counts: Vec<(String, usize)>,
    path_counts: Vec<(String, usize)>,
    diffstat: Option<String>,
    representative_commits: Vec<RepresentativeCommit>,
    note: Option<String>,
}

#[derive(Debug, Clone)]
struct RepresentativeCommit {
    heading: String,
    excerpt: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdatesLlmOutput {
    overview: String,
    #[serde(default)]
    themes: Vec<UpdatesTheme>,
    #[serde(default)]
    watch_items: Vec<String>,
    #[serde(default)]
    notable_commits: Vec<NotableCommit>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdatesTheme {
    title: String,
    summary: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    commit_refs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NotableCommit {
    commit: String,
    why_it_matters: String,
}

pub fn run_cli(cmd: UpdatesCommand) -> Result<()> {
    match run_with_overrides(cmd, RuntimeOverrides::default())? {
        RunOutcome::Written {
            output_path,
            markdown: _,
            commit_count: _,
        } => {
            println!("{}", output_path.display());
        }
        RunOutcome::Printed {
            markdown,
            commit_count: _,
        } => {
            print!("{markdown}");
        }
        RunOutcome::NoChanges { message } => {
            println!("{message}");
        }
    }
    Ok(())
}

fn run_with_overrides(cmd: UpdatesCommand, overrides: RuntimeOverrides) -> Result<RunOutcome> {
    let mut target = resolve_target(&cmd, &overrides)?;
    let generated_at = overrides.now.unwrap_or_else(Utc::now);
    let selection = select_delta(
        &target.repo_root,
        &target.tip_commit,
        &target.pathspecs,
        &cmd,
        &target.scope_state,
    )?;

    if selection.commits.is_empty() {
        let message = format!(
            "No new updates for {} on {} (tip {}).",
            target.repo_slug,
            target.ref_name,
            short_sha(&target.tip_commit)
        );
        if !cmd.stdout {
            let tip_commit = target.tip_commit.clone();
            record_scope_run(
                &mut target,
                UpdateRunRecord {
                    generated_at: generated_at.to_rfc3339(),
                    mode: selection.mode.as_str().to_string(),
                    from_commit: selection.from_commit.clone(),
                    from_label: selection.from_label.clone(),
                    to_commit: tip_commit,
                    commit_count: 0,
                    output_path: None,
                },
            )?;
        }
        return Ok(RunOutcome::NoChanges { message });
    }

    let evidence = build_evidence(&target, selection, generated_at)?;
    let summary = run_codex_summary(&evidence, cmd.model.as_deref(), &overrides)?;
    let markdown = render_markdown(&evidence, &summary);

    if cmd.stdout {
        return Ok(RunOutcome::Printed {
            markdown,
            commit_count: evidence.commits.len(),
        });
    }

    let output_path = write_markdown(&evidence, &markdown, &overrides)?;
    record_scope_run(
        &mut target,
        UpdateRunRecord {
            generated_at: evidence.generated_at.to_rfc3339(),
            mode: evidence.mode.as_str().to_string(),
            from_commit: evidence.from_commit.clone(),
            from_label: evidence.from_label.clone(),
            to_commit: evidence.tip_commit.clone(),
            commit_count: evidence.commits.len(),
            output_path: Some(output_path.display().to_string()),
        },
    )?;

    Ok(RunOutcome::Written {
        output_path,
        markdown,
        commit_count: evidence.commits.len(),
    })
}

fn resolve_target(cmd: &UpdatesCommand, overrides: &RuntimeOverrides) -> Result<RepoTarget> {
    let repo_input = cmd.repo.clone().unwrap_or(std::env::current_dir()?);
    let repo_root = resolve_repo_root(&repo_input)?;
    let pathspecs = normalize_pathspecs(&cmd.pathspecs);
    let remote = if cmd.git_ref.is_some() {
        None
    } else {
        Some(resolve_remote(&repo_root, cmd.remote.as_deref())?)
    };

    if cmd.fetch
        && let Some(remote_name) = remote.as_deref()
    {
        run_git(&repo_root, &["fetch", "--prune", remote_name])?;
    }

    let ref_name = if let Some(explicit) = cmd.git_ref.as_deref() {
        explicit.trim().to_string()
    } else {
        resolve_default_ref(&repo_root, remote.as_deref())?
    };

    let tip_commit = git_capture_in(&repo_root, &["rev-parse", &ref_name])?
        .trim()
        .to_string();
    if tip_commit.is_empty() {
        bail!("could not resolve commit for {}", ref_name);
    }

    let repo_slug = resolve_repo_slug(&repo_root, remote.as_deref()).unwrap_or_else(|| {
        repo_root
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });
    let scope_key = build_scope_key(&ref_name, &pathspecs);
    let index_path = updates_index_path(&repo_root, overrides)?;
    let mut index = load_index(&index_path)?;
    if index.version != INDEX_VERSION {
        index = UpdatesIndex::default();
    }
    if index.repo_root.is_empty() {
        index.repo_root = repo_root.display().to_string();
    }
    if index.repo_slug.is_empty() {
        index.repo_slug = repo_slug.clone();
    }

    let scope_state = index
        .scopes
        .get(&scope_key)
        .cloned()
        .unwrap_or(UpdateScopeIndex {
            ref_name: ref_name.clone(),
            pathspecs: pathspecs.clone(),
            runs: Vec::new(),
        });

    Ok(RepoTarget {
        repo_root,
        repo_slug,
        remote,
        ref_name,
        tip_commit,
        pathspecs,
        scope_key,
        scope_state,
        index_path,
        index,
    })
}

fn resolve_repo_root(path: &Path) -> Result<PathBuf> {
    let start = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let root = git_capture_in(&start, &["rev-parse", "--show-toplevel"])?;
    let trimmed = root.trim();
    if trimmed.is_empty() {
        bail!("{} is not inside a git repository", path.display());
    }
    Ok(PathBuf::from(trimmed))
}

fn resolve_remote(repo_root: &Path, requested: Option<&str>) -> Result<String> {
    if let Some(remote) = requested {
        let trimmed = remote.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let remotes = git_capture_in(repo_root, &["remote"])?;
    let mut items: Vec<String> = remotes
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if items.is_empty() {
        bail!("no git remotes configured");
    }
    if items.iter().any(|item| item == "origin") {
        return Ok("origin".to_string());
    }
    items.sort();
    Ok(items.remove(0))
}

fn resolve_default_ref(repo_root: &Path, remote: Option<&str>) -> Result<String> {
    let remote = remote.context("missing remote name")?;
    let symbolic = git_capture_in(
        repo_root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            &format!("refs/remotes/{remote}/HEAD"),
        ],
    )
    .unwrap_or_default();
    let symbolic = symbolic.trim();
    if !symbolic.is_empty() {
        return Ok(symbolic.to_string());
    }

    for candidate in ["main", "master"] {
        let full = format!("refs/remotes/{remote}/{candidate}");
        if git_success(repo_root, &["show-ref", "--verify", "--quiet", &full]) {
            return Ok(format!("{remote}/{candidate}"));
        }
    }

    let branches = git_capture_in(
        repo_root,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            &format!("refs/remotes/{remote}"),
        ],
    )?;
    for branch in branches
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if branch.ends_with("/HEAD") {
            continue;
        }
        return Ok(branch.to_string());
    }

    bail!("could not determine default remote branch for {}", remote)
}

fn resolve_repo_slug(repo_root: &Path, remote: Option<&str>) -> Option<String> {
    let remote = remote?;
    let url = git_capture_in(repo_root, &["remote", "get-url", remote]).ok()?;
    repo_slug_from_remote_url(url.trim()).or_else(|| {
        repo_root
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
    })
}

fn repo_slug_from_remote_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let last = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches(".git")
        .trim();
    if last.is_empty() {
        None
    } else {
        Some(last.to_string())
    }
}

fn normalize_pathspecs(pathspecs: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for pathspec in pathspecs {
        let trimmed = pathspec.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }
    out
}

fn build_scope_key(ref_name: &str, pathspecs: &[String]) -> String {
    if pathspecs.is_empty() {
        return format!("{ref_name}::all");
    }
    let mut items = pathspecs.to_vec();
    items.sort();
    format!("{ref_name}::{}", items.join("|"))
}

fn select_delta(
    repo_root: &Path,
    tip_commit: &str,
    pathspecs: &[String],
    cmd: &UpdatesCommand,
    scope_state: &UpdateScopeIndex,
) -> Result<DeltaSelection> {
    let previous = scope_state.runs.last().cloned();
    let previous_output_path = previous.as_ref().and_then(|run| run.output_path.clone());
    let days = cmd.days.max(1);

    let (mode, from_commit, from_label, note, since_spec) = if let Some(raw_since) =
        cmd.since.as_deref()
    {
        let trimmed = raw_since.trim();
        if trimmed.is_empty() {
            bail!("--since cannot be empty");
        }
        match resolve_commitish(repo_root, trimmed) {
            Ok(commit) => (
                SelectionMode::ExplicitSince,
                Some(commit.clone()),
                short_sha(&commit),
                None,
                None,
            ),
            Err(_) => (
                SelectionMode::ExplicitSince,
                None,
                trimmed.to_string(),
                None,
                Some(trimmed.to_string()),
            ),
        }
    } else if !cmd.force {
        if let Some(last) = previous.as_ref() {
            if last.to_commit == tip_commit {
                return Ok(DeltaSelection {
                    mode: SelectionMode::Incremental,
                    from_commit: Some(last.to_commit.clone()),
                    from_label: short_sha(&last.to_commit),
                    commits: Vec::new(),
                    previous_output_path,
                    note: None,
                    diff_base: Some(last.to_commit.clone()),
                });
            }

            if git_is_ancestor(repo_root, &last.to_commit, tip_commit) {
                (
                    SelectionMode::Incremental,
                    Some(last.to_commit.clone()),
                    short_sha(&last.to_commit),
                    None,
                    None,
                )
            } else {
                let merge_base =
                    git_capture_in(repo_root, &["merge-base", &last.to_commit, tip_commit])?
                        .trim()
                        .to_string();
                if merge_base.is_empty() {
                    (
                        SelectionMode::HistoryRewritten,
                        None,
                        format!("last {days} days"),
                        Some(format!(
                            "History changed since the last recorded update and no merge-base was available. Falling back to the last {} days instead of the previous tip {}.",
                            days,
                            short_sha(&last.to_commit)
                        )),
                        Some(format!("{days} days ago")),
                    )
                } else {
                    (
                        SelectionMode::HistoryRewritten,
                        Some(merge_base.clone()),
                        short_sha(&merge_base),
                        Some(format!(
                            "History changed since the last recorded update. The summary starts from merge-base {} instead of the previous tip {}.",
                            short_sha(&merge_base),
                            short_sha(&last.to_commit)
                        )),
                        None,
                    )
                }
            }
        } else {
            (
                SelectionMode::InitialLookback,
                None,
                format!("last {days} days"),
                None,
                Some(format!("{days} days ago")),
            )
        }
    } else {
        (
            SelectionMode::ForcedLookback,
            None,
            format!("last {days} days"),
            None,
            Some(format!("{days} days ago")),
        )
    };

    let commits = collect_commits(
        repo_root,
        tip_commit,
        from_commit.as_deref(),
        since_spec.as_deref(),
        pathspecs,
    )?;
    let diff_base = from_commit.clone().or_else(|| {
        commits
            .first()
            .and_then(|first| first_parent(repo_root, &first.sha).ok())
    });

    Ok(DeltaSelection {
        mode,
        from_commit,
        from_label,
        commits,
        previous_output_path,
        note,
        diff_base,
    })
}

fn resolve_commitish(repo_root: &Path, value: &str) -> Result<String> {
    let resolved = git_capture_in(repo_root, &["rev-parse", &format!("{value}^{{commit}}")])?;
    let trimmed = resolved.trim();
    if trimmed.is_empty() {
        bail!("could not resolve commitish {}", value);
    }
    Ok(trimmed.to_string())
}

fn collect_commits(
    repo_root: &Path,
    tip_commit: &str,
    from_commit: Option<&str>,
    since_spec: Option<&str>,
    pathspecs: &[String],
) -> Result<Vec<CommitRecord>> {
    let mut args = vec![
        "log".to_string(),
        "--reverse".to_string(),
        "--date=short".to_string(),
        "--pretty=format:%H%x1f%h%x1f%an%x1f%ad%x1f%s%x1e".to_string(),
    ];

    if let Some(from_commit) = from_commit {
        args.push(format!("{from_commit}..{tip_commit}"));
    } else {
        if let Some(since_spec) = since_spec {
            args.push(format!("--since={since_spec}"));
        }
        args.push(tip_commit.to_string());
    }

    if !pathspecs.is_empty() {
        args.push("--".to_string());
        args.extend(pathspecs.iter().cloned());
    }

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = git_capture_in(repo_root, &arg_refs)?;
    let mut commits = Vec::new();

    for chunk in output.split('\u{1e}') {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut lines = trimmed.lines();
        let header = lines.next().unwrap_or_default();
        let fields: Vec<&str> = header.split('\u{1f}').collect();
        if fields.len() < 5 {
            continue;
        }
        let files: Vec<String> = lines
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        commits.push(CommitRecord {
            sha: fields[0].to_string(),
            short_sha: fields[1].to_string(),
            author: fields[2].to_string(),
            date: fields[3].to_string(),
            subject: fields[4].to_string(),
            files,
        });
    }

    Ok(commits)
}

fn first_parent(repo_root: &Path, sha: &str) -> Result<String> {
    let parent = git_capture_in(repo_root, &["rev-parse", &format!("{sha}^")])?;
    let trimmed = parent.trim();
    if trimmed.is_empty() {
        bail!("{} has no parent", sha);
    }
    Ok(trimmed.to_string())
}

fn build_evidence(
    target: &RepoTarget,
    selection: DeltaSelection,
    generated_at: DateTime<Utc>,
) -> Result<UpdateEvidence> {
    let author_counts = count_by_key(selection.commits.iter().map(|commit| commit.author.clone()));
    let path_counts = count_by_key(
        selection
            .commits
            .iter()
            .flat_map(|commit| commit.files.iter().map(|path| path_bucket(path))),
    );
    let diffstat = match selection.diff_base.as_deref() {
        Some(base) => collect_diffstat(
            &target.repo_root,
            base,
            &target.tip_commit,
            &target.pathspecs,
        )?,
        None => None,
    };
    let representative_commits =
        collect_representative_commits(&target.repo_root, &selection.commits, &target.pathspecs)?;
    let oldest_date = selection.commits.first().map(|commit| commit.date.clone());
    let newest_date = selection.commits.last().map(|commit| commit.date.clone());

    Ok(UpdateEvidence {
        generated_at,
        repo_root: target.repo_root.clone(),
        repo_slug: target.repo_slug.clone(),
        remote: target.remote.clone(),
        ref_name: target.ref_name.clone(),
        tip_commit: target.tip_commit.clone(),
        pathspecs: target.pathspecs.clone(),
        mode: selection.mode,
        from_commit: selection.from_commit,
        from_label: selection.from_label,
        previous_output_path: selection.previous_output_path,
        commits: selection.commits,
        oldest_date,
        newest_date,
        author_counts,
        path_counts,
        diffstat,
        representative_commits,
        note: selection.note,
    })
}

fn count_by_key<I>(items: I) -> Vec<(String, usize)>
where
    I: IntoIterator<Item = String>,
{
    let mut counts: HashMap<String, usize> = HashMap::new();
    for item in items {
        *counts.entry(item).or_insert(0) += 1;
    }
    let mut items: Vec<(String, usize)> = counts.into_iter().collect();
    items.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    items
}

fn path_bucket(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return ".".to_string();
    }
    match parts[0] {
        "x" if parts.len() >= 2 => format!("x/{}", parts[1]),
        "arena" | "ide" | "infra" | "docs" | "src" | "crates" | "lib" if parts.len() >= 2 => {
            format!("{}/{}", parts[0], parts[1])
        }
        value => value.to_string(),
    }
}

fn collect_diffstat(
    repo_root: &Path,
    base: &str,
    tip: &str,
    pathspecs: &[String],
) -> Result<Option<String>> {
    let mut args = vec![
        "diff".to_string(),
        "--stat=140,100".to_string(),
        base.to_string(),
        tip.to_string(),
    ];
    if !pathspecs.is_empty() {
        args.push("--".to_string());
        args.extend(pathspecs.iter().cloned());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = git_capture_in(repo_root, &arg_refs)?;
    let trimmed = truncate_chars(output.trim(), MAX_DIFFSTAT_CHARS);
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed))
    }
}

fn collect_representative_commits(
    repo_root: &Path,
    commits: &[CommitRecord],
    pathspecs: &[String],
) -> Result<Vec<RepresentativeCommit>> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }

    let mut ranked = commits.to_vec();
    ranked.sort_by(|left, right| {
        right
            .file_count()
            .cmp(&left.file_count())
            .then_with(|| right.date.cmp(&left.date))
            .then_with(|| right.sha.cmp(&left.sha))
    });

    let mut seen = BTreeSet::new();
    let mut selected = Vec::new();
    for commit in ranked {
        if !seen.insert(commit.sha.clone()) {
            continue;
        }
        selected.push(commit);
        if selected.len() >= MAX_REPRESENTATIVE_COMMITS {
            break;
        }
    }

    selected.sort_by(|left, right| {
        left.date
            .cmp(&right.date)
            .then_with(|| left.sha.cmp(&right.sha))
    });

    let mut rendered = Vec::new();
    for commit in selected {
        let excerpt = collect_commit_excerpt(repo_root, &commit.sha, pathspecs)?;
        rendered.push(RepresentativeCommit {
            heading: format!(
                "{} {} ({}, {} files)",
                commit.short_sha,
                commit.subject,
                commit.date,
                commit.file_count()
            ),
            excerpt,
        });
    }
    Ok(rendered)
}

fn collect_commit_excerpt(repo_root: &Path, sha: &str, pathspecs: &[String]) -> Result<String> {
    let mut args = vec![
        "show".to_string(),
        "--no-ext-diff".to_string(),
        "--date=short".to_string(),
        "--stat=140,100".to_string(),
        "--summary".to_string(),
        "--format=commit %H%nshort: %h%nauthor: %an%ndate: %ad%nsubject: %s".to_string(),
        "--unified=1".to_string(),
        sha.to_string(),
    ];
    if !pathspecs.is_empty() {
        args.push("--".to_string());
        args.extend(pathspecs.iter().cloned());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = git_capture_in(repo_root, &arg_refs)?;
    Ok(truncate_chars(output.trim(), MAX_REPRESENTATIVE_CHARS))
}

fn run_codex_summary(
    evidence: &UpdateEvidence,
    model: Option<&str>,
    overrides: &RuntimeOverrides,
) -> Result<UpdatesLlmOutput> {
    let prompt = build_codex_prompt(evidence);
    let codex_bin = overrides
        .codex_bin
        .clone()
        .unwrap_or_else(|| configured_codex_bin_for_workdir(&evidence.repo_root));

    let mut schema_file = NamedTempFile::new().context("failed to create temporary schema file")?;
    let mut output_file = NamedTempFile::new().context("failed to create temporary output file")?;
    let schema = summary_output_schema();
    schema_file
        .write_all(
            serde_json::to_string_pretty(&schema)
                .context("failed to encode output schema")?
                .as_bytes(),
        )
        .context("failed to write output schema")?;
    output_file
        .write_all(b"")
        .context("failed to initialize output file")?;

    let mut command = Command::new(&codex_bin);
    command
        .current_dir(&evidence.repo_root)
        .arg("exec")
        .arg("--ephemeral")
        .arg("--sandbox")
        .arg("read-only")
        .arg("--output-schema")
        .arg(schema_file.path())
        .arg("--output-last-message")
        .arg(output_file.path())
        .arg("-C")
        .arg(&evidence.repo_root);
    if let Some(model) = model {
        let trimmed = model.trim();
        if !trimmed.is_empty() {
            command.arg("-m").arg(trimmed);
        }
    }
    command
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to spawn Codex binary '{}' for repository updates",
            codex_bin
        )
    })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .context("failed to write prompt to Codex stdin")?;
    }

    let output = child
        .wait_with_output()
        .context("failed while waiting for Codex summary process")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        bail!(
            "codex exec failed (status {:?})\nstderr: {}\nstdout: {}",
            output.status.code(),
            stderr,
            stdout
        );
    }

    let last_message = fs::read_to_string(output_file.path())
        .context("failed to read Codex summary output file")?;
    parse_summary_output(&last_message)
}

fn summary_output_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "overview": { "type": "string" },
            "themes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "summary": { "type": "string" },
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "commitRefs": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    },
                    "required": ["title", "summary", "paths", "commitRefs"],
                    "additionalProperties": false
                }
            },
            "watchItems": {
                "type": "array",
                "items": { "type": "string" }
            },
            "notableCommits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "commit": { "type": "string" },
                        "whyItMatters": { "type": "string" }
                    },
                    "required": ["commit", "whyItMatters"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["overview", "themes", "watchItems", "notableCommits"],
        "additionalProperties": false
    })
}

fn build_codex_prompt(evidence: &UpdateEvidence) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are generating a high-signal engineering update for a repository.\n\
         Use only the evidence below. Do not invent details and do not run tools.\n\
         Prefer a small number of meaningful themes over a laundry list.\n\
         Focus on behavior changes, platform or infrastructure shifts, workflow changes, risks, and commits that materially matter.\n\
         Ignore mechanical churn unless it changes behavior.\n\
         Mention exact dates when the timing matters.\n\
         Keep the overview tight.\n\
         Keep watchItems sparse and concrete.\n\n",
    );

    writeln!(&mut prompt, "Repo: {}", evidence.repo_slug).ok();
    writeln!(&mut prompt, "Repo root: {}", evidence.repo_root.display()).ok();
    writeln!(
        &mut prompt,
        "Remote: {}",
        evidence.remote.as_deref().unwrap_or("<none>")
    )
    .ok();
    writeln!(&mut prompt, "Ref: {}", evidence.ref_name).ok();
    writeln!(&mut prompt, "Tip commit: {}", evidence.tip_commit).ok();
    writeln!(&mut prompt, "Mode: {}", evidence.mode.as_str()).ok();
    writeln!(
        &mut prompt,
        "Range start: {}",
        evidence
            .from_commit
            .as_deref()
            .map(short_sha)
            .unwrap_or_else(|| evidence.from_label.clone())
    )
    .ok();
    writeln!(&mut prompt, "Commit count: {}", evidence.commits.len()).ok();
    if let (Some(oldest), Some(newest)) = (
        evidence.oldest_date.as_deref(),
        evidence.newest_date.as_deref(),
    ) {
        writeln!(&mut prompt, "Date span: {} to {}", oldest, newest).ok();
    }
    if !evidence.pathspecs.is_empty() {
        writeln!(
            &mut prompt,
            "Path filters: {}",
            evidence.pathspecs.join(", ")
        )
        .ok();
    }
    if let Some(previous) = evidence.previous_output_path.as_deref() {
        writeln!(&mut prompt, "Previous update: {}", previous).ok();
    }
    if let Some(note) = evidence.note.as_deref() {
        writeln!(&mut prompt, "Special note: {}", note).ok();
    }

    prompt.push_str("\nAggregate evidence\n");
    writeln!(
        &mut prompt,
        "Top authors: {}",
        render_counts_inline(&evidence.author_counts, MAX_COUNT_LINES)
    )
    .ok();
    writeln!(
        &mut prompt,
        "Top paths: {}",
        render_counts_inline(&evidence.path_counts, MAX_COUNT_LINES)
    )
    .ok();

    if let Some(diffstat) = evidence.diffstat.as_deref() {
        prompt.push_str("\nCombined diffstat\n```text\n");
        prompt.push_str(diffstat);
        prompt.push_str("\n```\n");
    }

    prompt.push_str("\nChronological commit subjects\n");
    for line in render_commit_subject_lines(&evidence.commits) {
        prompt.push_str("- ");
        prompt.push_str(&line);
        prompt.push('\n');
    }

    if !evidence.representative_commits.is_empty() {
        prompt.push_str("\nRepresentative commits\n");
        for commit in &evidence.representative_commits {
            prompt.push_str("```text\n");
            prompt.push_str(&commit.heading);
            prompt.push('\n');
            prompt.push_str(&commit.excerpt);
            prompt.push_str("\n```\n");
        }
    }

    prompt.push_str(
        "\nOutput requirements\n\
         - overview: 2-4 sentences.\n\
         - themes: 3-6 themes when evidence supports it.\n\
         - Each theme should group related changes and explain why they matter.\n\
         - watchItems: only include concrete follow-ups, risks, or things worth monitoring.\n\
         - notableCommits: include only commits that materially changed product behavior, infra, or workflow.\n",
    );

    prompt
}

fn render_commit_subject_lines(commits: &[CommitRecord]) -> Vec<String> {
    if commits.len() <= MAX_SUBJECTS {
        return commits.iter().map(CommitRecord::ref_line).collect();
    }

    let head = MAX_SUBJECTS / 2;
    let tail = MAX_SUBJECTS - head;
    let mut lines: Vec<String> = commits
        .iter()
        .take(head)
        .map(CommitRecord::ref_line)
        .collect();
    lines.push(format!(
        "... {} additional commits omitted ...",
        commits.len().saturating_sub(MAX_SUBJECTS)
    ));
    lines.extend(
        commits
            .iter()
            .skip(commits.len().saturating_sub(tail))
            .map(CommitRecord::ref_line),
    );
    lines
}

fn render_counts_inline(counts: &[(String, usize)], limit: usize) -> String {
    if counts.is_empty() {
        return "none".to_string();
    }
    counts
        .iter()
        .take(limit)
        .map(|(name, count)| format!("{name} ({count})"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_summary_output(raw: &str) -> Result<UpdatesLlmOutput> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Codex returned an empty summary payload");
    }

    if let Ok(parsed) = serde_json::from_str::<UpdatesLlmOutput>(trimmed) {
        return Ok(parsed);
    }

    if let Some(inner) = strip_json_code_fence(trimmed)
        && let Ok(parsed) = serde_json::from_str::<UpdatesLlmOutput>(inner)
    {
        return Ok(parsed);
    }

    bail!(
        "failed to parse Codex summary output as JSON: {}",
        truncate_chars(trimmed, 600)
    )
}

fn strip_json_code_fence(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if !trimmed.starts_with("```") {
        return None;
    }
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))?;
    let body = body.trim_start_matches('\n');
    body.strip_suffix("```").map(str::trim)
}

fn render_markdown(evidence: &UpdateEvidence, summary: &UpdatesLlmOutput) -> String {
    let mut body = String::new();
    body.push_str(&render_frontmatter(evidence));
    writeln!(&mut body, "# Updates: {}", evidence.repo_slug).ok();
    writeln!(
        &mut body,
        "\nGenerated on {} from `{}` at `{}`.",
        evidence.generated_at.to_rfc3339(),
        evidence.ref_name,
        short_sha(&evidence.tip_commit)
    )
    .ok();

    body.push_str("\n## Overview\n");
    body.push_str(summary.overview.trim());
    body.push('\n');

    if !summary.themes.is_empty() {
        body.push_str("\n## Themes\n");
        for theme in &summary.themes {
            if theme.title.trim().is_empty() || theme.summary.trim().is_empty() {
                continue;
            }
            writeln!(&mut body, "\n### {}", theme.title.trim()).ok();
            body.push_str(theme.summary.trim());
            body.push('\n');
            if !theme.paths.is_empty() {
                body.push_str("\nPaths:\n");
                for path in &theme.paths {
                    if path.trim().is_empty() {
                        continue;
                    }
                    writeln!(&mut body, "- `{}`", path.trim()).ok();
                }
            }
            if !theme.commit_refs.is_empty() {
                body.push_str("\nEvidence:\n");
                for commit in &theme.commit_refs {
                    if commit.trim().is_empty() {
                        continue;
                    }
                    writeln!(&mut body, "- `{}`", commit.trim()).ok();
                }
            }
        }
    }

    if !summary.watch_items.is_empty() {
        body.push_str("\n## Watch\n");
        for item in &summary.watch_items {
            if item.trim().is_empty() {
                continue;
            }
            writeln!(&mut body, "- {}", item.trim()).ok();
        }
    }

    if !summary.notable_commits.is_empty() {
        body.push_str("\n## Notable Commits\n");
        for commit in &summary.notable_commits {
            if commit.commit.trim().is_empty() || commit.why_it_matters.trim().is_empty() {
                continue;
            }
            writeln!(
                &mut body,
                "- `{}`: {}",
                commit.commit.trim(),
                commit.why_it_matters.trim()
            )
            .ok();
        }
    }

    body.push_str("\n## Evidence Snapshot\n");
    writeln!(&mut body, "- Commit count: {}", evidence.commits.len()).ok();
    if let (Some(oldest), Some(newest)) = (
        evidence.oldest_date.as_deref(),
        evidence.newest_date.as_deref(),
    ) {
        writeln!(&mut body, "- Date span: {} to {}", oldest, newest).ok();
    }
    writeln!(
        &mut body,
        "- Top authors: {}",
        render_counts_inline(&evidence.author_counts, MAX_COUNT_LINES)
    )
    .ok();
    writeln!(
        &mut body,
        "- Top paths: {}",
        render_counts_inline(&evidence.path_counts, MAX_COUNT_LINES)
    )
    .ok();
    if let Some(previous) = evidence.previous_output_path.as_deref() {
        writeln!(&mut body, "- Previous update: `{}`", previous).ok();
    }
    if let Some(note) = evidence.note.as_deref() {
        writeln!(&mut body, "- Note: {}", note).ok();
    }

    if let Some(diffstat) = evidence.diffstat.as_deref() {
        body.push_str("\nDiffstat\n```text\n");
        body.push_str(diffstat);
        body.push_str("\n```\n");
    }

    if !evidence.representative_commits.is_empty() {
        body.push_str("\nRepresentative commits\n");
        for commit in &evidence.representative_commits {
            body.push_str("```text\n");
            body.push_str(&commit.heading);
            body.push('\n');
            body.push_str(&commit.excerpt);
            body.push_str("\n```\n");
        }
    }

    body
}

fn render_frontmatter(evidence: &UpdateEvidence) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    yaml_line(&mut out, "repo", &evidence.repo_slug);
    yaml_line(
        &mut out,
        "repo_root",
        &evidence.repo_root.display().to_string(),
    );
    yaml_line(
        &mut out,
        "generated_at",
        &evidence.generated_at.to_rfc3339(),
    );
    yaml_line(&mut out, "remote", evidence.remote.as_deref().unwrap_or(""));
    yaml_line(&mut out, "ref", &evidence.ref_name);
    yaml_line(&mut out, "tip_commit", &evidence.tip_commit);
    yaml_line(&mut out, "mode", evidence.mode.as_str());
    yaml_line(&mut out, "from_label", &evidence.from_label);
    yaml_line(
        &mut out,
        "from_commit",
        evidence.from_commit.as_deref().unwrap_or(""),
    );
    out.push_str(&format!("commit_count: {}\n", evidence.commits.len()));
    if !evidence.pathspecs.is_empty() {
        out.push_str("paths:\n");
        for pathspec in &evidence.pathspecs {
            out.push_str("  - ");
            out.push_str(&yaml_quote(pathspec));
            out.push('\n');
        }
    }
    out.push_str("---\n\n");
    out
}

fn yaml_line(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(": ");
    out.push_str(&yaml_quote(value));
    out.push('\n');
}

fn yaml_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn write_markdown(
    evidence: &UpdateEvidence,
    markdown: &str,
    overrides: &RuntimeOverrides,
) -> Result<PathBuf> {
    let root = updates_output_root(overrides);
    let target_dir = root.join(&evidence.repo_slug);
    fs::create_dir_all(&target_dir)
        .with_context(|| format!("failed to create {}", target_dir.display()))?;
    let filename = format!("{}.md", evidence.generated_at.format("%Y-%m-%dT%H-%M-%SZ"));
    let output_path = target_dir.join(filename);
    fs::write(&output_path, markdown)
        .with_context(|| format!("failed to write {}", output_path.display()))?;
    Ok(output_path)
}

fn record_scope_run(target: &mut RepoTarget, record: UpdateRunRecord) -> Result<()> {
    let scope = target
        .index
        .scopes
        .entry(target.scope_key.clone())
        .or_insert_with(|| UpdateScopeIndex {
            ref_name: target.ref_name.clone(),
            pathspecs: target.pathspecs.clone(),
            runs: Vec::new(),
        });

    scope.ref_name = target.ref_name.clone();
    scope.pathspecs = target.pathspecs.clone();
    scope.runs.push(record);
    if scope.runs.len() > MAX_SCOPE_HISTORY {
        let drain = scope.runs.len().saturating_sub(MAX_SCOPE_HISTORY);
        scope.runs.drain(0..drain);
    }

    target.index.version = INDEX_VERSION;
    target.index.repo_root = target.repo_root.display().to_string();
    target.index.repo_slug = target.repo_slug.clone();
    save_index(&target.index_path, &target.index)
}

fn updates_output_root(overrides: &RuntimeOverrides) -> PathBuf {
    overrides
        .output_root
        .clone()
        .unwrap_or_else(|| config::expand_path(DEFAULT_OUTPUT_ROOT))
}

fn updates_state_root(overrides: &RuntimeOverrides) -> Result<PathBuf> {
    let root = if let Some(path) = overrides.state_root.clone() {
        path
    } else {
        config::ensure_global_state_dir()?.join("updates")
    };
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;
    Ok(root)
}

fn updates_index_path(repo_root: &Path, overrides: &RuntimeOverrides) -> Result<PathBuf> {
    let canonical = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let digest = blake3::hash(canonical.to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    let dir = updates_state_root(overrides)?.join(digest);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir.join("index.json"))
}

fn load_index(path: &Path) -> Result<UpdatesIndex> {
    if !path.exists() {
        return Ok(UpdatesIndex::default());
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let parsed = serde_json::from_str::<UpdatesIndex>(&text)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(parsed)
}

fn save_index(path: &Path, index: &UpdatesIndex) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(index).context("failed to encode updates index")?;
    let tmp_path = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(&tmp_path, data)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path)
        .or_else(|_| {
            let _ = fs::remove_file(path);
            fs::rename(&tmp_path, path)
        })
        .with_context(|| format!("failed to finalize {}", path.display()))?;
    Ok(())
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut iter = value.chars();
    let collected: String = iter.by_ref().take(max_chars).collect();
    if iter.next().is_some() {
        format!("{collected}\n... (truncated)")
    } else {
        collected
    }
}

fn git_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_git(repo_root: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}

fn git_success(repo_root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn git_is_ancestor(repo_root: &Path, older: &str, newer: &str) -> bool {
    Command::new("git")
        .current_dir(repo_root)
        .args(["merge-base", "--is-ancestor", older, newer])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .expect("git command should spawn");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent should exist");
        }
        fs::write(path, contents).expect("file should write");
    }

    fn create_commit(repo: &Path, rel: &str, contents: &str, message: &str) -> String {
        write_file(&repo.join(rel), contents);
        git(repo, &["add", rel]);
        git(repo, &["commit", "-m", message]);
        git_capture_in(repo, &["rev-parse", "HEAD"])
            .expect("rev-parse should work")
            .trim()
            .to_string()
    }

    fn create_mock_codex(path: &Path) {
        let script = r#"#!/bin/sh
out=""
while [ $# -gt 0 ]; do
  case "$1" in
    --output-last-message)
      out="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done
cat >/dev/null
cat <<'JSON' >"$out"
{"overview":"Recent work focused on the update command flow.","themes":[{"title":"CLI and state management","summary":"A new update workflow writes structured repository summaries and persists delta state for future runs.","paths":["src/updates.rs","src/cli.rs"],"commitRefs":["abcdef0 add updates command","1234567 wire updates runner"]}],"watchItems":["Validate summary quality on very large monorepos."],"notableCommits":[{"commit":"1234567 wire updates runner","whyItMatters":"It connects the persistent update flow end to end."}]}
JSON
"#;
        write_file(path, script);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).expect("metadata").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).expect("chmod");
        }
    }

    #[test]
    fn repo_slug_parser_handles_common_remote_urls() {
        assert_eq!(
            repo_slug_from_remote_url("git@github.com:openai/codex.git"),
            Some("codex".to_string())
        );
        assert_eq!(
            repo_slug_from_remote_url("https://github.com/mark3labs/kit"),
            Some("kit".to_string())
        );
    }

    #[test]
    fn scope_key_is_stable_for_path_order() {
        let left = build_scope_key("origin/main", &["src".to_string(), "docs".to_string()]);
        let right = build_scope_key("origin/main", &["docs".to_string(), "src".to_string()]);
        assert_eq!(left, right);
    }

    #[test]
    fn parse_summary_output_accepts_fenced_json() {
        let parsed = parse_summary_output(
            "```json\n{\"overview\":\"ok\",\"themes\":[],\"watchItems\":[],\"notableCommits\":[]}\n```",
        )
        .expect("json should parse");
        assert_eq!(parsed.overview, "ok");
    }

    #[test]
    fn updates_command_writes_markdown_and_index() {
        let repo = TempDir::new().expect("temp repo");
        git(repo.path(), &["init"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);

        let first = create_commit(repo.path(), "src/main.rs", "fn main() {}\n", "add main");
        let _second = create_commit(
            repo.path(),
            "src/updates.rs",
            "pub fn updates() {}\n",
            "wire updates runner",
        );

        let mock_codex = repo.path().join("mock-codex.sh");
        create_mock_codex(&mock_codex);

        let output_root = repo.path().join("out");
        let state_root = repo.path().join("state");
        let now = DateTime::parse_from_rfc3339("2026-03-19T12:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc);

        let outcome = run_with_overrides(
            UpdatesCommand {
                repo: Some(repo.path().to_path_buf()),
                git_ref: Some("HEAD".to_string()),
                remote: None,
                pathspecs: Vec::new(),
                since: Some(first),
                days: 14,
                fetch: false,
                stdout: false,
                force: false,
                model: Some("gpt-test".to_string()),
            },
            RuntimeOverrides {
                codex_bin: Some(mock_codex.display().to_string()),
                output_root: Some(output_root.clone()),
                state_root: Some(state_root.clone()),
                now: Some(now),
            },
        )
        .expect("updates should run");

        let output_path = match outcome {
            RunOutcome::Written { output_path, .. } => output_path,
            other => panic!("unexpected outcome: {other:?}"),
        };

        let markdown = fs::read_to_string(&output_path).expect("markdown should exist");
        assert!(markdown.contains("# Updates:"));
        assert!(markdown.contains("CLI and state management"));
        assert!(markdown.contains("## Evidence Snapshot"));

        let entries: Vec<PathBuf> = fs::read_dir(&state_root)
            .expect("state root")
            .filter_map(|entry| entry.ok().map(|item| item.path()))
            .collect();
        assert_eq!(entries.len(), 1, "one repo state dir expected");
        let index_path = entries[0].join("index.json");
        let index = fs::read_to_string(&index_path).expect("index should exist");
        assert!(index.contains("\"commit_count\": 1"));

        let second_outcome = run_with_overrides(
            UpdatesCommand {
                repo: Some(repo.path().to_path_buf()),
                git_ref: Some("HEAD".to_string()),
                remote: None,
                pathspecs: Vec::new(),
                since: None,
                days: 14,
                fetch: false,
                stdout: false,
                force: false,
                model: None,
            },
            RuntimeOverrides {
                codex_bin: Some(mock_codex.display().to_string()),
                output_root: Some(output_root),
                state_root: Some(state_root),
                now: Some(
                    DateTime::parse_from_rfc3339("2026-03-19T13:00:00Z")
                        .expect("timestamp")
                        .with_timezone(&Utc),
                ),
            },
        )
        .expect("second updates run should succeed");

        match second_outcome {
            RunOutcome::NoChanges { .. } => {}
            other => panic!("expected no-change outcome, got {other:?}"),
        }
    }
}
