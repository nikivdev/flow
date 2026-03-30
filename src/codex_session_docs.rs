use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::bail;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{activity_log, codex_skill_eval, config};

const SESSION_DOC_PACKET_VERSION: u32 = 1;
const SESSION_DOC_INDEX_VERSION: u32 = 1;
const DOC_REVIEW_QUEUE_VERSION: u32 = 1;
const DOC_QUALITY_LOG_VERSION: u32 = 1;
const MAX_INDEX_RECENT: usize = 200;
const MAX_INDEX_FILE_SESSIONS: usize = 8;
const SIGNAL_SCAN_LIMIT: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionDocPatchChange {
    pub path: String,
    pub action: String,
    pub patch: String,
}

#[derive(Debug, Clone)]
pub struct CompletedSessionDocInput {
    pub session_id: String,
    pub session_file: PathBuf,
    pub target_path: String,
    pub launch_path: Option<String>,
    pub first_user_prompt: Option<String>,
    pub completion_summary: String,
    pub completed_at_unix: u64,
    pub patch_changes: Vec<SessionDocPatchChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionDocOutcomeRef {
    pub kind: String,
    pub success: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_path: Option<String>,
    pub recorded_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionDocPacket {
    pub version: u32,
    pub provider: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    pub target_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_path: Option<String>,
    pub session_file_path: String,
    pub storage_class: String,
    pub recorded_at_unix: u64,
    pub completed_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_user_prompt: Option<String>,
    pub completion_summary: String,
    pub changed_files: Vec<String>,
    pub patch_changes: Vec<SessionDocPatchChange>,
    pub attributed_patch_files: Vec<String>,
    pub inferred_repo_files: Vec<String>,
    pub artifact_paths: Vec<String>,
    pub activity_event_ids: Vec<String>,
    pub outcome_refs: Vec<SessionDocOutcomeRef>,
    pub confidence: String,
    pub documentation_mode: String,
    pub summary_path: String,
    pub diff_path: String,
    pub doc_review_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub promoted_paths: Vec<String>,
    pub committed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionDocPromotion {
    pub version: u32,
    pub recorded_at_unix: u64,
    pub session_id: String,
    pub storage_class: String,
    pub doc_review_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub promoted_paths: Vec<String>,
    pub promoted: bool,
    pub committed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionDocIndexEntry {
    pub session_id: String,
    pub session_key: String,
    pub completed_at_unix: u64,
    pub confidence: String,
    pub documentation_mode: String,
    pub changed_files: Vec<String>,
    pub completion_summary: String,
    pub summary_path: String,
    pub diff_path: String,
    pub session_json_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SessionDocIndex {
    version: u32,
    generated_at_unix: u64,
    total_sessions: usize,
    recent_sessions: Vec<SessionDocIndexEntry>,
    by_file: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DocReviewQueueEntry {
    pub version: u32,
    pub enqueued_at_unix: u64,
    pub session_id: String,
    pub session_key: String,
    pub target_root: String,
    pub storage_class: String,
    pub confidence: String,
    pub documentation_mode: String,
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    pub summary: String,
    pub session_json_path: String,
    pub summary_path: String,
    pub diff_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_eligible: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<String>,
    pub commit_eligible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub promoted_paths: Vec<String>,
    pub doc_review_state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromotionDecision {
    promotion_eligible: bool,
    promotion_target: String,
    promotion_reason: String,
    review_decision: String,
    commit_eligible: bool,
    commit_decision: String,
    blocked_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionPromotionPreview {
    pub session_id: String,
    pub session_key: String,
    pub target_path: String,
    pub eligible: bool,
    pub review_state: String,
    pub promotion_reason: String,
    pub blocked_reason: Option<String>,
    pub markdown: String,
}

#[derive(Debug, Clone)]
pub struct CommitPendingPlan {
    pub session_keys: Vec<String>,
    pub files: Vec<String>,
    pub commit_message: String,
    pub committed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DocQualityEvent {
    pub version: u32,
    pub recorded_at_unix: u64,
    pub phase: String,
    pub session_id: String,
    pub session_key: String,
    pub target_root: String,
    pub storage_class: String,
    pub confidence: String,
    pub doc_review_state: String,
    pub changed_file_count: usize,
    pub promoted: bool,
    pub committed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_target: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub promoted_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
}

#[derive(Debug, Clone)]
struct SessionLinkedSignals {
    event_ids: Vec<String>,
    artifact_paths: Vec<String>,
    outcomes: Vec<SessionDocOutcomeRef>,
    runtime_token: Option<String>,
    trace_id: Option<String>,
}

pub fn document_completed_session(
    input: &CompletedSessionDocInput,
) -> Result<Option<SessionDocPacket>> {
    let target_root = detect_project_root(Path::new(&input.target_path))
        .unwrap_or_else(|| normalize_target_root(Path::new(&input.target_path)));
    let state_root = config::ensure_global_state_dir()?;
    document_completed_session_at(&target_root, &state_root, input).map(Some)
}

pub fn recent_packets(project_root: &Path, limit: usize) -> Result<Vec<SessionDocIndexEntry>> {
    let state_root = config::ensure_global_state_dir()?;
    recent_packets_at(project_root, &state_root, limit)
}

fn recent_packets_at(
    project_root: &Path,
    state_root: &Path,
    limit: usize,
) -> Result<Vec<SessionDocIndexEntry>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut merged = Vec::new();
    let mut seen = BTreeSet::new();
    for index_path in [
        session_changes_root(state_root, project_root).join("index.json"),
        legacy_session_changes_root(project_root).join("index.json"),
    ] {
        if !index_path.exists() {
            continue;
        }
        let bytes = fs::read(&index_path)
            .with_context(|| format!("failed to read {}", index_path.display()))?;
        let index: SessionDocIndex = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to decode {}", index_path.display()))?;
        for entry in index.recent_sessions {
            if seen.insert(entry.session_key.clone()) {
                merged.push(entry);
            }
        }
    }
    merged.sort_by(|left, right| {
        right
            .completed_at_unix
            .cmp(&left.completed_at_unix)
            .then_with(|| right.session_key.cmp(&left.session_key))
    });
    merged.truncate(limit);
    Ok(merged)
}

pub fn pending_review_entries(
    project_root: &Path,
    limit: usize,
) -> Result<Vec<DocReviewQueueEntry>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let queue_path = doc_review_queue_path(&config::ensure_global_state_dir()?);
    load_review_queue_for_project(&queue_path, project_root, limit)
}

pub fn review_pending_entries(limit: usize) -> Result<usize> {
    if limit == 0 {
        return Ok(0);
    }
    let state_root = config::ensure_global_state_dir()?;
    let queue_path = doc_review_queue_path(&state_root);
    let mut entries = load_review_queue_entries(&queue_path)?;
    let mut updated = 0usize;
    for entry in entries.iter_mut() {
        if entry.doc_review_state != "pending" {
            continue;
        }
        if updated >= limit {
            break;
        }
        review_queue_entry_in_place(&state_root, entry)?;
        updated += 1;
    }
    write_review_queue_entries(&queue_path, &entries)?;
    Ok(updated)
}

pub fn promote_session(
    project_root: &Path,
    session_hint: &str,
    apply: bool,
) -> Result<SessionPromotionPreview> {
    let state_root = config::ensure_global_state_dir()?;
    let queue_path = doc_review_queue_path(&state_root);
    let mut entries = load_review_queue_entries(&queue_path)?;
    let entry_index = resolve_queue_entry_index(&entries, project_root, session_hint)?;
    review_queue_entry_in_place(&state_root, &mut entries[entry_index])?;
    let mut packet = load_packet(Path::new(&entries[entry_index].session_json_path))?;
    let session_key = entries[entry_index].session_key.clone();
    let target_path = entries[entry_index]
        .promotion_target
        .clone()
        .or_else(|| packet.promotion_target.clone())
        .unwrap_or_else(|| promoted_doc_relative_path(&session_key, &packet));
    let markdown = render_promoted_markdown(&packet);
    let preview = SessionPromotionPreview {
        session_id: packet.session_id.clone(),
        session_key: session_key.clone(),
        target_path: target_path.clone(),
        eligible: entries[entry_index].promotion_eligible.unwrap_or(false),
        review_state: entries[entry_index].doc_review_state.clone(),
        promotion_reason: entries[entry_index]
            .promotion_reason
            .clone()
            .unwrap_or_else(|| "no promotion reason recorded".to_string()),
        blocked_reason: entries[entry_index].blocked_reason.clone(),
        markdown,
    };
    if !apply {
        write_review_queue_entries(&queue_path, &entries)?;
        return Ok(preview);
    }
    if !preview.eligible {
        bail!(
            "{}",
            preview
                .blocked_reason
                .clone()
                .unwrap_or_else(|| "session is not eligible for promotion".to_string())
        );
    }

    let promoted_path = resolve_project_relative_path(project_root, &target_path);
    if let Some(parent) = promoted_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&promoted_path, &preview.markdown)
        .with_context(|| format!("failed to write {}", promoted_path.display()))?;

    let promoted_path_string = promoted_path.display().to_string();
    if !entries[entry_index]
        .promoted_paths
        .iter()
        .any(|path| path == &promoted_path_string)
    {
        entries[entry_index]
            .promoted_paths
            .push(promoted_path_string.clone());
    }
    entries[entry_index].doc_review_state = "promoted".to_string();
    entries[entry_index].review_decision = Some("promoted".to_string());
    entries[entry_index].commit_eligible = true;
    entries[entry_index].commit_decision = Some("pending".to_string());
    entries[entry_index].blocked_reason = None;

    packet.doc_review_state = "promoted".to_string();
    packet.promotion_target = Some(target_path.clone());
    packet.blocked_reason = None;
    if !packet
        .promoted_paths
        .iter()
        .any(|path| path == &promoted_path_string)
    {
        packet.promoted_paths.push(promoted_path_string.clone());
    }
    write_packet(Path::new(&entries[entry_index].session_json_path), &packet)?;
    write_promotion_state(
        &promotion_path_for_session_json(Path::new(&entries[entry_index].session_json_path)),
        &packet,
    )?;
    append_quality_event(&state_root, "promoted", &packet)?;
    write_review_queue_entries(&queue_path, &entries)?;

    Ok(SessionPromotionPreview {
        session_id: preview.session_id,
        session_key: preview.session_key,
        target_path,
        eligible: true,
        review_state: "promoted".to_string(),
        promotion_reason: preview.promotion_reason,
        blocked_reason: None,
        markdown: preview.markdown,
    })
}

pub fn commit_pending(project_root: &Path, dry_run: bool) -> Result<Option<CommitPendingPlan>> {
    let state_root = config::ensure_global_state_dir()?;
    let queue_path = doc_review_queue_path(&state_root);
    let mut entries = load_review_queue_entries(&queue_path)?;
    let project_root_display = project_root.display().to_string();
    let candidate_indexes = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.target_root == project_root_display
                && entry.doc_review_state == "promoted"
                && entry.commit_eligible
                && !entry.promoted_paths.is_empty()
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    if candidate_indexes.is_empty() {
        return Ok(None);
    }

    let changed_paths = git_changed_paths(project_root)?;
    if changed_paths.is_empty() {
        return Ok(None);
    }

    let mut allowed_paths = BTreeSet::new();
    let mut session_keys = Vec::new();
    for index in &candidate_indexes {
        let entry = &entries[*index];
        session_keys.push(entry.session_key.clone());
        for path in allowed_commit_paths(project_root, entry) {
            allowed_paths.insert(path);
        }
    }

    let unexpected = changed_paths
        .iter()
        .filter(|path| !allowed_paths.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unexpected.is_empty() {
        bail!(
            "refusing doc commit because unrelated changes are present: {}",
            unexpected.join(", ")
        );
    }

    let files = changed_paths
        .into_iter()
        .filter(|path| allowed_paths.contains(path.as_str()))
        .collect::<Vec<_>>();
    if files.is_empty() {
        return Ok(None);
    }

    let commit_message = build_doc_commit_message(&entries, &candidate_indexes);
    let plan = CommitPendingPlan {
        session_keys: session_keys.clone(),
        files: files.clone(),
        commit_message: commit_message.clone(),
        committed: false,
    };

    if dry_run {
        return Ok(Some(plan));
    }

    git_add_paths(project_root, &files)?;
    git_commit_paths(project_root, &commit_message)?;

    for index in candidate_indexes {
        let entry = &mut entries[index];
        entry.doc_review_state = "committed".to_string();
        entry.commit_decision = Some("committed".to_string());
        let session_json_path = Path::new(&entry.session_json_path);
        let mut packet = load_packet(session_json_path)?;
        packet.doc_review_state = "committed".to_string();
        packet.committed = true;
        packet.commit_message = Some(commit_message.clone());
        write_packet(session_json_path, &packet)?;
        write_promotion_state(&promotion_path_for_session_json(session_json_path), &packet)?;
        append_quality_event(&state_root, "committed", &packet)?;
    }

    write_review_queue_entries(&queue_path, &entries)?;

    Ok(Some(CommitPendingPlan {
        session_keys,
        files,
        commit_message,
        committed: true,
    }))
}

fn document_completed_session_at(
    target_root: &Path,
    state_root: &Path,
    input: &CompletedSessionDocInput,
) -> Result<SessionDocPacket> {
    let recorded_at_unix = unix_now();
    let session_key = format!(
        "{}-{}",
        short_session_id(&input.session_id),
        input.completed_at_unix
    );
    let bundle_dir = session_bundle_dir(
        state_root,
        target_root,
        input.completed_at_unix,
        &session_key,
    );
    fs::create_dir_all(&bundle_dir)
        .with_context(|| format!("failed to create {}", bundle_dir.display()))?;

    let signals = gather_session_signals(target_root, &input.session_id, recorded_at_unix)?;
    let changed_files = unique_paths(
        input
            .patch_changes
            .iter()
            .map(|change| change.path.as_str()),
    );
    let attributed_patch_files = changed_files.clone();
    let confidence = compute_confidence(&changed_files, &signals);
    let documentation_mode = documentation_mode_for(&confidence);

    let summary_path = bundle_dir.join("summary.md");
    let diff_path = bundle_dir.join("diff.txt");
    let session_json_path = bundle_dir.join("session.json");
    let promotion_path = bundle_dir.join("promotion.json");

    let packet = SessionDocPacket {
        version: SESSION_DOC_PACKET_VERSION,
        provider: "codex".to_string(),
        session_id: input.session_id.clone(),
        runtime_token: signals.runtime_token.clone(),
        trace_id: signals.trace_id.clone(),
        target_path: target_root.display().to_string(),
        launch_path: input
            .launch_path
            .clone()
            .or_else(|| Some(input.target_path.clone())),
        session_file_path: input.session_file.display().to_string(),
        storage_class: "project_ai_docs".to_string(),
        recorded_at_unix,
        completed_at_unix: input.completed_at_unix,
        first_user_prompt: input.first_user_prompt.clone(),
        completion_summary: input.completion_summary.clone(),
        changed_files: changed_files.clone(),
        patch_changes: input.patch_changes.clone(),
        attributed_patch_files,
        inferred_repo_files: Vec::new(),
        artifact_paths: signals.artifact_paths.clone(),
        activity_event_ids: signals.event_ids.clone(),
        outcome_refs: signals.outcomes.clone(),
        confidence: confidence.clone(),
        documentation_mode: documentation_mode.clone(),
        summary_path: summary_path.display().to_string(),
        diff_path: diff_path.display().to_string(),
        doc_review_state: "pending".to_string(),
        promotion_target: None,
        promotion_reason: None,
        blocked_reason: None,
        reviewed_at_unix: None,
        promoted_paths: Vec::new(),
        committed: false,
        commit_message: None,
    };

    fs::write(&summary_path, render_summary_markdown(&packet))
        .with_context(|| format!("failed to write {}", summary_path.display()))?;
    fs::write(&diff_path, render_diff_text(&packet))
        .with_context(|| format!("failed to write {}", diff_path.display()))?;
    fs::write(
        &session_json_path,
        serde_json::to_vec_pretty(&packet).context("failed to encode session doc packet")?,
    )
    .with_context(|| format!("failed to write {}", session_json_path.display()))?;

    let promotion = SessionDocPromotion {
        version: SESSION_DOC_PACKET_VERSION,
        recorded_at_unix,
        session_id: input.session_id.clone(),
        storage_class: "project_ai_docs".to_string(),
        doc_review_state: "pending".to_string(),
        promotion_target: None,
        promotion_reason: None,
        blocked_reason: None,
        reviewed_at_unix: None,
        promoted_at_unix: None,
        promoted_paths: Vec::new(),
        promoted: false,
        committed: false,
        commit_message: None,
    };
    fs::write(
        &promotion_path,
        serde_json::to_vec_pretty(&promotion).context("failed to encode promotion state")?,
    )
    .with_context(|| format!("failed to write {}", promotion_path.display()))?;

    update_project_index(state_root, target_root, &packet, &session_json_path)?;
    enqueue_review_item(
        state_root,
        target_root,
        &packet,
        &session_key,
        &session_json_path,
    )?;
    append_quality_event(state_root, "captured", &packet)?;
    emit_docs_activity_event(
        target_root,
        &packet,
        changed_files.len(),
        &summary_path,
        &session_json_path,
    )?;

    Ok(packet)
}

fn render_summary_markdown(packet: &SessionDocPacket) -> String {
    let mut out = String::new();
    out.push_str("# Session Change Summary\n\n");

    out.push_str("## Prompt\n\n");
    if let Some(prompt) = packet.first_user_prompt.as_deref() {
        out.push_str(prompt.trim());
        out.push('\n');
    } else {
        out.push_str("No first user prompt was captured.\n");
    }
    out.push('\n');

    out.push_str("## Outcome\n\n");
    out.push_str(&format!(
        "- Summary: {}\n",
        packet.completion_summary.trim()
    ));
    out.push_str(&format!("- Confidence: {}\n", packet.confidence));
    out.push_str(&format!(
        "- Documentation mode: {}\n",
        packet.documentation_mode
    ));
    out.push_str("- Attribution source: apply_patch-derived file changes only\n\n");

    out.push_str("## What Changed\n\n");
    if packet.patch_changes.is_empty() {
        out.push_str(
            "No authoritative patch-attributed file changes were recorded for this session.\n\n",
        );
    } else {
        for change in &packet.patch_changes {
            out.push_str(&format!(
                "- {} `{}`\n",
                capitalize_action(&change.action),
                display_repo_relative(&packet.target_path, &change.path)
            ));
        }
        out.push('\n');
    }

    out.push_str("## Files\n\n");
    if packet.changed_files.is_empty() {
        out.push_str("- None\n\n");
    } else {
        for path in &packet.changed_files {
            out.push_str(&format!(
                "- `{}`\n",
                display_repo_relative(&packet.target_path, path)
            ));
        }
        out.push('\n');
    }

    out.push_str("## Validation Signals\n\n");
    if packet.activity_event_ids.is_empty() && packet.outcome_refs.is_empty() {
        out.push_str("- No linked validation signals were recorded.\n\n");
    } else {
        if !packet.activity_event_ids.is_empty() {
            out.push_str(&format!(
                "- Activity events: {}\n",
                packet.activity_event_ids.join(", ")
            ));
        }
        for outcome in &packet.outcome_refs {
            out.push_str(&format!(
                "- Outcome `{}` success `{:.2}`{}\n",
                outcome.kind,
                outcome.success,
                outcome
                    .artifact_path
                    .as_deref()
                    .map(|path| format!(" ({})", path))
                    .unwrap_or_default()
            ));
        }
        out.push('\n');
    }

    out.push_str("## Risks / Unknowns\n\n");
    if packet.patch_changes.is_empty() {
        out.push_str("- This is a metadata-only or partial packet. Shell/editor-only changes are not claimed.\n");
    } else {
        out.push_str("- Only `apply_patch`-attributed changes are claimed here. Shell/editor changes may be missing.\n");
    }
    out.push_str("- No transcript was embedded. Load the session or trace on demand if deeper context is needed.\n\n");

    out.push_str("## Trace / Session Metadata\n\n");
    out.push_str(&format!("- Session: `{}`\n", packet.session_id));
    if let Some(trace_id) = packet.trace_id.as_deref() {
        out.push_str(&format!("- Trace: `{}`\n", trace_id));
    } else {
        out.push_str("- Trace: not linked\n");
    }
    if let Some(runtime_token) = packet.runtime_token.as_deref() {
        out.push_str(&format!("- Runtime token: `{}`\n", runtime_token));
    }
    out.push_str(&format!(
        "- Completed: `{}`\n",
        format_unix_secs(packet.completed_at_unix)
    ));
    out.push_str(&format!("- Session file: `{}`\n", packet.session_file_path));

    out
}

fn render_diff_text(packet: &SessionDocPacket) -> String {
    if packet.patch_changes.is_empty() {
        return "No apply_patch diff excerpts were attributed to this session.\n".to_string();
    }

    let mut out = String::new();
    out.push_str("# Session Diff Excerpts\n\n");
    for change in &packet.patch_changes {
        out.push_str(&format!(
            "=== {} {} ===\n",
            change.action.to_ascii_uppercase(),
            display_repo_relative(&packet.target_path, &change.path)
        ));
        if change.patch.trim().is_empty() {
            out.push_str("(empty patch)\n\n");
            continue;
        }
        out.push_str(change.patch.trim());
        out.push_str("\n\n");
    }
    out
}

fn update_project_index(
    state_root: &Path,
    target_root: &Path,
    packet: &SessionDocPacket,
    session_json_path: &Path,
) -> Result<()> {
    let index_path = session_changes_root(state_root, target_root).join("index.json");
    let mut index = if index_path.exists() {
        let bytes = fs::read(&index_path)
            .with_context(|| format!("failed to read {}", index_path.display()))?;
        serde_json::from_slice::<SessionDocIndex>(&bytes).unwrap_or_else(|_| SessionDocIndex {
            version: SESSION_DOC_INDEX_VERSION,
            generated_at_unix: unix_now(),
            total_sessions: 0,
            recent_sessions: Vec::new(),
            by_file: BTreeMap::new(),
        })
    } else {
        SessionDocIndex {
            version: SESSION_DOC_INDEX_VERSION,
            generated_at_unix: unix_now(),
            total_sessions: 0,
            recent_sessions: Vec::new(),
            by_file: BTreeMap::new(),
        }
    };

    let session_key = session_key_from_packet(packet);
    let entry = SessionDocIndexEntry {
        session_id: packet.session_id.clone(),
        session_key: session_key.clone(),
        completed_at_unix: packet.completed_at_unix,
        confidence: packet.confidence.clone(),
        documentation_mode: packet.documentation_mode.clone(),
        changed_files: packet.changed_files.clone(),
        completion_summary: packet.completion_summary.clone(),
        summary_path: packet.summary_path.clone(),
        diff_path: packet.diff_path.clone(),
        session_json_path: session_json_path.display().to_string(),
    };

    index
        .recent_sessions
        .retain(|item| item.session_key != session_key);
    index.recent_sessions.push(entry);
    index.recent_sessions.sort_by(|left, right| {
        right
            .completed_at_unix
            .cmp(&left.completed_at_unix)
            .then_with(|| right.session_key.cmp(&left.session_key))
    });
    index.recent_sessions.truncate(MAX_INDEX_RECENT);
    index.total_sessions = index.recent_sessions.len();
    index.generated_at_unix = unix_now();

    for path in &packet.changed_files {
        let key = display_repo_relative(&packet.target_path, path);
        let file_entries = index.by_file.entry(key).or_default();
        if !file_entries.iter().any(|value| value == &session_key) {
            file_entries.insert(0, session_key.clone());
            file_entries.truncate(MAX_INDEX_FILE_SESSIONS);
        }
    }

    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(
        &index_path,
        serde_json::to_vec_pretty(&index).context("failed to encode session docs index")?,
    )
    .with_context(|| format!("failed to write {}", index_path.display()))?;
    Ok(())
}

fn enqueue_review_item(
    state_root: &Path,
    target_root: &Path,
    packet: &SessionDocPacket,
    session_key: &str,
    session_json_path: &Path,
) -> Result<()> {
    let queue_path = doc_review_queue_path(state_root);
    if let Some(parent) = queue_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&queue_path)
        .with_context(|| format!("failed to open {}", queue_path.display()))?;
    let entry = DocReviewQueueEntry {
        version: DOC_REVIEW_QUEUE_VERSION,
        enqueued_at_unix: unix_now(),
        session_id: packet.session_id.clone(),
        session_key: session_key.to_string(),
        target_root: target_root.display().to_string(),
        storage_class: packet.storage_class.clone(),
        confidence: packet.confidence.clone(),
        documentation_mode: packet.documentation_mode.clone(),
        changed_files: packet.changed_files.clone(),
        trace_id: packet.trace_id.clone(),
        summary: packet.completion_summary.clone(),
        session_json_path: session_json_path.display().to_string(),
        summary_path: packet.summary_path.clone(),
        diff_path: packet.diff_path.clone(),
        promotion_eligible: None,
        promotion_target: None,
        promotion_reason: None,
        review_decision: None,
        commit_eligible: packet.confidence == "high",
        commit_decision: None,
        blocked_reason: None,
        reviewed_at_unix: None,
        promoted_paths: Vec::new(),
        doc_review_state: "pending".to_string(),
    };
    serde_json::to_writer(&mut file, &entry).context("failed to encode doc review queue entry")?;
    file.write_all(b"\n")
        .context("failed to terminate doc review queue entry")?;
    Ok(())
}

fn review_queue_entry_in_place(state_root: &Path, entry: &mut DocReviewQueueEntry) -> Result<()> {
    let reviewed_at_unix = unix_now();
    let session_json_path = Path::new(&entry.session_json_path);
    let mut packet = match load_packet(session_json_path) {
        Ok(packet) => packet,
        Err(err) => {
            entry.promotion_eligible = Some(false);
            entry.review_decision = Some("blocked".to_string());
            entry.commit_eligible = false;
            entry.commit_decision = Some("blocked".to_string());
            entry.blocked_reason = Some(format!("failed to load packet: {err:#}"));
            entry.doc_review_state = "blocked".to_string();
            entry.reviewed_at_unix = Some(reviewed_at_unix);
            return Ok(());
        }
    };
    let decision = build_promotion_decision(Path::new(&entry.target_root), &packet);
    entry.promotion_eligible = Some(decision.promotion_eligible);
    entry.promotion_target = Some(decision.promotion_target.clone());
    entry.promotion_reason = Some(decision.promotion_reason.clone());
    entry.review_decision = Some(decision.review_decision.clone());
    entry.commit_eligible = decision.commit_eligible;
    entry.commit_decision = Some(decision.commit_decision.clone());
    entry.blocked_reason = decision.blocked_reason.clone();
    entry.reviewed_at_unix = Some(reviewed_at_unix);
    entry.doc_review_state = if decision.promotion_eligible {
        "reviewed".to_string()
    } else {
        "blocked".to_string()
    };

    packet.doc_review_state = entry.doc_review_state.clone();
    packet.promotion_target = Some(decision.promotion_target);
    packet.promotion_reason = entry.promotion_reason.clone();
    packet.blocked_reason = entry.blocked_reason.clone();
    packet.reviewed_at_unix = entry.reviewed_at_unix;
    write_packet(session_json_path, &packet)?;
    write_promotion_state(&promotion_path_for_session_json(session_json_path), &packet)?;
    append_quality_event(state_root, "reviewed", &packet)?;
    Ok(())
}

fn build_promotion_decision(target_root: &Path, packet: &SessionDocPacket) -> PromotionDecision {
    let promoted_target = promoted_doc_relative_path(&session_key_from_packet(packet), packet);
    let summary_ok = summary_is_non_trivial(&packet.completion_summary);
    let docs_writable = promoted_docs_root(target_root)
        .parent()
        .map(|path| fs::create_dir_all(path).is_ok())
        .unwrap_or(false);

    let mut blocked_reasons = Vec::new();
    if packet.confidence != "high" {
        blocked_reasons.push(format!("confidence is {}", packet.confidence));
    }
    if packet.documentation_mode != "packet_with_patch" {
        blocked_reasons.push(format!(
            "documentation mode is {}",
            packet.documentation_mode
        ));
    }
    if packet.attributed_patch_files.is_empty() {
        blocked_reasons.push("no attributed patch files were recorded".to_string());
    }
    if !summary_ok {
        blocked_reasons.push("summary is too short or vague".to_string());
    }
    if !target_root.exists() {
        blocked_reasons.push("target root no longer exists".to_string());
    }
    if !docs_writable {
        blocked_reasons.push("docs root is not writable".to_string());
    }

    let topic = promotion_topic(packet);
    if blocked_reasons.is_empty() {
        PromotionDecision {
            promotion_eligible: true,
            promotion_target: promoted_target,
            promotion_reason: format!("promote as durable project AI docs for {}", topic),
            review_decision: "reviewed".to_string(),
            commit_eligible: true,
            commit_decision: "pending".to_string(),
            blocked_reason: None,
        }
    } else {
        PromotionDecision {
            promotion_eligible: false,
            promotion_target: promoted_target,
            promotion_reason: format!("keep as packet-only because {}", blocked_reasons.join("; ")),
            review_decision: "blocked".to_string(),
            commit_eligible: false,
            commit_decision: "blocked".to_string(),
            blocked_reason: Some(blocked_reasons.join("; ")),
        }
    }
}

fn promotion_topic(packet: &SessionDocPacket) -> &'static str {
    if packet.changed_files.iter().any(|path| {
        path.contains("/src/cli")
            || path.contains("/src/help")
            || path.contains("/routes/")
            || path.ends_with("/cli.rs")
    }) {
        return "CLI or workflow behavior";
    }
    if packet.changed_files.iter().any(|path| {
        path.contains("daemon")
            || path.contains("codexd")
            || path.contains("ops")
            || path.ends_with("/server.rs")
    }) {
        return "daemon or ops behavior";
    }
    "local session knowledge"
}

fn promoted_doc_relative_path(session_key: &str, packet: &SessionDocPacket) -> String {
    let slug = slugify(
        packet
            .first_user_prompt
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(packet.completion_summary.as_str()),
        48,
    );
    format!(
        ".ai/docs/session-changes-promoted/{}-{}.md",
        session_key, slug
    )
}

fn promoted_docs_root(project_root: &Path) -> PathBuf {
    project_root
        .join(".ai")
        .join("docs")
        .join("session-changes-promoted")
}

fn resolve_project_relative_path(project_root: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        project_root.join(candidate)
    }
}

fn summary_is_non_trivial(summary: &str) -> bool {
    let trimmed = summary.trim();
    if trimmed.len() < 20 {
        return false;
    }
    trimmed.split_whitespace().count() >= 3
}

fn render_promoted_markdown(packet: &SessionDocPacket) -> String {
    let mut out = String::new();
    out.push_str("# Codex Session Change Note\n\n");
    out.push_str("## Summary\n\n");
    out.push_str(packet.completion_summary.trim());
    out.push_str("\n\n");

    out.push_str("## Context\n\n");
    if let Some(prompt) = packet.first_user_prompt.as_deref() {
        out.push_str("- Prompt: ");
        out.push_str(prompt.trim());
        out.push('\n');
    }
    out.push_str(&format!("- Session: `{}`\n", packet.session_id));
    if let Some(trace_id) = packet.trace_id.as_deref() {
        out.push_str(&format!("- Trace: `{}`\n", trace_id));
    }
    out.push_str(&format!(
        "- Completed: `{}`\n",
        format_unix_secs(packet.completed_at_unix)
    ));
    out.push('\n');

    out.push_str("## Files\n\n");
    if packet.changed_files.is_empty() {
        out.push_str("- None\n\n");
    } else {
        for path in &packet.changed_files {
            out.push_str(&format!(
                "- `{}`\n",
                display_repo_relative(&packet.target_path, path)
            ));
        }
        out.push('\n');
    }

    out.push_str("## Why This Was Captured\n\n");
    out.push_str("- This note was promoted from a daemon-captured session packet because the session had high-confidence attributed patch changes.\n");
    out.push_str(
        "- The note stays local to `.ai/docs/` and avoids embedding full transcript content.\n\n",
    );

    out.push_str("## Retrieval Hints\n\n");
    out.push_str("- Load the original packet if you need patch excerpts or queue metadata.\n");
    out.push_str("- Use the session or trace ids above to fetch deeper context on demand.\n");
    out.push_str(
        "- Prefer this note for Codex retrieval before opening raw session transcripts.\n",
    );

    out
}

fn load_packet(session_json_path: &Path) -> Result<SessionDocPacket> {
    let bytes = fs::read(session_json_path)
        .with_context(|| format!("failed to read {}", session_json_path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode {}", session_json_path.display()))
}

fn write_packet(session_json_path: &Path, packet: &SessionDocPacket) -> Result<()> {
    fs::write(
        session_json_path,
        serde_json::to_vec_pretty(packet).context("failed to encode session doc packet")?,
    )
    .with_context(|| format!("failed to write {}", session_json_path.display()))
}

fn write_promotion_state(promotion_path: &Path, packet: &SessionDocPacket) -> Result<()> {
    let promotion = SessionDocPromotion {
        version: SESSION_DOC_PACKET_VERSION,
        recorded_at_unix: packet.recorded_at_unix,
        session_id: packet.session_id.clone(),
        storage_class: packet.storage_class.clone(),
        doc_review_state: packet.doc_review_state.clone(),
        promotion_target: packet.promotion_target.clone(),
        promotion_reason: packet.promotion_reason.clone(),
        blocked_reason: packet.blocked_reason.clone(),
        reviewed_at_unix: packet.reviewed_at_unix,
        promoted_at_unix: if packet.promoted_paths.is_empty() {
            None
        } else {
            Some(unix_now())
        },
        promoted_paths: packet.promoted_paths.clone(),
        promoted: !packet.promoted_paths.is_empty(),
        committed: packet.committed,
        commit_message: packet.commit_message.clone(),
    };
    fs::write(
        promotion_path,
        serde_json::to_vec_pretty(&promotion).context("failed to encode promotion state")?,
    )
    .with_context(|| format!("failed to write {}", promotion_path.display()))
}

fn promotion_path_for_session_json(session_json_path: &Path) -> PathBuf {
    session_json_path
        .parent()
        .map(|path| path.join("promotion.json"))
        .unwrap_or_else(|| PathBuf::from("promotion.json"))
}

fn load_review_queue_entries(queue_path: &Path) -> Result<Vec<DocReviewQueueEntry>> {
    if !queue_path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(queue_path)
        .with_context(|| format!("failed to read {}", queue_path.display()))?;
    let mut entries = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<DocReviewQueueEntry>(trimmed) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

fn write_review_queue_entries(queue_path: &Path, entries: &[DocReviewQueueEntry]) -> Result<()> {
    if let Some(parent) = queue_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut bytes = Vec::new();
    for entry in entries {
        let mut line = serde_json::to_vec(entry).context("failed to encode review queue entry")?;
        bytes.append(&mut line);
        bytes.push(b'\n');
    }
    fs::write(queue_path, bytes)
        .with_context(|| format!("failed to write {}", queue_path.display()))
}

fn resolve_queue_entry_index(
    entries: &[DocReviewQueueEntry],
    project_root: &Path,
    session_hint: &str,
) -> Result<usize> {
    let project_root = project_root.display().to_string();
    let matches = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.target_root == project_root
                && (entry.session_id == session_hint
                    || entry.session_key == session_hint
                    || entry.session_id.starts_with(session_hint)
                    || entry.session_key.starts_with(session_hint))
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [index] => Ok(*index),
        [] => bail!("no session-doc queue entry matches `{session_hint}`"),
        _ => bail!("multiple session-doc queue entries match `{session_hint}`"),
    }
}

fn git_changed_paths(project_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(project_root)
        .output()
        .context("failed to run git status")?;
    if !output.status.success() {
        bail!("git status failed with {}", output.status);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut paths = Vec::new();
    for line in stdout.lines() {
        if line.len() < 4 {
            continue;
        }
        let raw_path = line[3..].trim();
        let path = raw_path
            .split_once(" -> ")
            .map(|(_, after)| after)
            .unwrap_or(raw_path);
        if !path.is_empty() {
            paths.push(path.to_string());
        }
    }
    Ok(dedupe_preserve_order(paths))
}

fn allowed_commit_paths(project_root: &Path, entry: &DocReviewQueueEntry) -> Vec<String> {
    let mut paths = Vec::new();
    for path in &entry.promoted_paths {
        paths.push(display_repo_relative(
            &project_root.display().to_string(),
            &resolve_project_relative_path(project_root, path)
                .display()
                .to_string(),
        ));
    }
    for path in [
        &entry.session_json_path,
        &entry.summary_path,
        &entry.diff_path,
    ] {
        paths.push(display_repo_relative(
            &project_root.display().to_string(),
            path,
        ));
    }
    paths.push(display_repo_relative(
        &project_root.display().to_string(),
        &promotion_path_for_session_json(Path::new(&entry.session_json_path))
            .display()
            .to_string(),
    ));
    dedupe_preserve_order(paths)
}

fn git_add_paths(project_root: &Path, paths: &[String]) -> Result<()> {
    let status = Command::new("git")
        .current_dir(project_root)
        .arg("add")
        .arg("--")
        .args(paths)
        .status()
        .context("failed to run git add")?;
    if !status.success() {
        bail!("git add failed with {}", status);
    }
    Ok(())
}

fn git_commit_paths(project_root: &Path, message: &str) -> Result<()> {
    let status = Command::new("git")
        .current_dir(project_root)
        .args(["commit", "-m", message])
        .status()
        .context("failed to run git commit")?;
    if !status.success() {
        bail!("git commit failed with {}", status);
    }
    Ok(())
}

fn build_doc_commit_message(
    entries: &[DocReviewQueueEntry],
    candidate_indexes: &[usize],
) -> String {
    if candidate_indexes.len() == 1 {
        let entry = &entries[candidate_indexes[0]];
        let topic = entry
            .changed_files
            .first()
            .map(|path| slugify(path, 40))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| entry.session_key.clone());
        return format!("docs(ai): capture codex session changes for {}", topic);
    }
    "docs(ai): capture codex session changes".to_string()
}

fn doc_quality_log_path(state_root: &Path) -> PathBuf {
    state_root.join("codex").join("doc-quality.jsonl")
}

fn append_quality_event(state_root: &Path, phase: &str, packet: &SessionDocPacket) -> Result<()> {
    let log_path = doc_quality_log_path(state_root);
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let event = DocQualityEvent {
        version: DOC_QUALITY_LOG_VERSION,
        recorded_at_unix: unix_now(),
        phase: phase.to_string(),
        session_id: packet.session_id.clone(),
        session_key: session_key_from_packet(packet),
        target_root: packet.target_path.clone(),
        storage_class: packet.storage_class.clone(),
        confidence: packet.confidence.clone(),
        doc_review_state: packet.doc_review_state.clone(),
        changed_file_count: packet.changed_files.len(),
        promoted: !packet.promoted_paths.is_empty(),
        committed: packet.committed,
        promotion_target: packet.promotion_target.clone(),
        promoted_paths: packet.promoted_paths.clone(),
        commit_message: packet.commit_message.clone(),
    };
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    serde_json::to_writer(&mut file, &event).context("failed to encode doc quality event")?;
    file.write_all(b"\n")
        .context("failed to terminate doc quality event")?;
    Ok(())
}

fn slugify(input: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars() {
        let next = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if next == '-' {
            if last_dash || out.is_empty() {
                continue;
            }
            last_dash = true;
        } else {
            last_dash = false;
        }
        out.push(next);
        if out.len() >= max_len {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "session-note".to_string()
    } else {
        out
    }
}

fn emit_docs_activity_event(
    target_root: &Path,
    packet: &SessionDocPacket,
    changed_file_count: usize,
    summary_path: &Path,
    session_json_path: &Path,
) -> Result<()> {
    let summary = if changed_file_count == 0 {
        format!(
            "captured metadata-only session packet for {}",
            short_session_id(&packet.session_id)
        )
    } else {
        format!(
            "documented session changes for {} ({} files)",
            short_session_id(&packet.session_id),
            changed_file_count
        )
    };
    let mut event = activity_log::ActivityEvent::changed("codex.docs.changed", summary);
    event.target_path = Some(target_root.display().to_string());
    event.launch_path = packet.launch_path.clone();
    event.session_id = Some(packet.session_id.clone());
    event.source = Some("codex-session-docs".to_string());
    event.artifact_path = Some(summary_path.display().to_string());
    event.payload_ref = Some(session_json_path.display().to_string());
    event.dedupe_key = Some(format!(
        "codex:docs:{}:{}",
        packet.session_id, packet.completed_at_unix
    ));
    activity_log::append_daily_event(event)
}

fn gather_session_signals(
    target_root: &Path,
    session_id: &str,
    _recorded_at_unix: u64,
) -> Result<SessionLinkedSignals> {
    let recent_events = activity_log::recent_events(SIGNAL_SCAN_LIMIT)?;
    let mut event_ids = Vec::new();
    let mut artifact_paths = Vec::new();
    for event in recent_events {
        if event.session_id.as_deref() != Some(session_id) {
            continue;
        }
        if let Some(target_path) = event.target_path.as_deref()
            && !path_starts_with_target(target_path, target_root)
        {
            continue;
        }
        event_ids.push(event.event_id);
        if let Some(path) = event.artifact_path {
            artifact_paths.push(path);
        }
        if let Some(path) = event.payload_ref {
            artifact_paths.push(path);
        }
    }

    let eval_events = codex_skill_eval::load_events(Some(target_root), SIGNAL_SCAN_LIMIT)?;
    let mut runtime_token = None;
    let mut trace_id = None;
    for event in eval_events {
        if event.session_id.as_deref() != Some(session_id) {
            continue;
        }
        if runtime_token.is_none() {
            runtime_token = event.runtime_token.clone();
        }
        if trace_id.is_none() {
            trace_id = event.trace_id.clone();
        }
    }

    let outcomes = codex_skill_eval::load_outcomes(Some(target_root), SIGNAL_SCAN_LIMIT)?
        .into_iter()
        .filter(|outcome| outcome.session_id.as_deref() == Some(session_id))
        .map(|outcome| {
            if let Some(path) = outcome.artifact_path.as_deref() {
                artifact_paths.push(path.to_string());
            }
            if trace_id.is_none() {
                trace_id = outcome.trace_id.clone();
            }
            if runtime_token.is_none() {
                runtime_token = outcome.runtime_token.clone();
            }
            SessionDocOutcomeRef {
                kind: outcome.kind,
                success: outcome.success,
                artifact_path: outcome.artifact_path,
                recorded_at_unix: outcome.recorded_at_unix,
            }
        })
        .collect::<Vec<_>>();

    Ok(SessionLinkedSignals {
        event_ids,
        artifact_paths: dedupe_preserve_order(artifact_paths),
        outcomes,
        runtime_token,
        trace_id,
    })
}

fn compute_confidence(changed_files: &[String], signals: &SessionLinkedSignals) -> String {
    if !changed_files.is_empty() {
        return "high".to_string();
    }
    if !signals.event_ids.is_empty()
        || !signals.outcomes.is_empty()
        || !signals.artifact_paths.is_empty()
    {
        return "medium".to_string();
    }
    "low".to_string()
}

fn documentation_mode_for(confidence: &str) -> String {
    match confidence {
        "high" => "packet_with_patch".to_string(),
        "medium" => "partial".to_string(),
        _ => "metadata_only".to_string(),
    }
}

fn normalize_target_root(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        if canonical.is_dir() {
            return canonical;
        }
        return canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(canonical);
    }
    if path.is_dir() {
        return path.to_path_buf();
    }
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf())
}

fn detect_project_root(path: &Path) -> Option<PathBuf> {
    let cwd = normalize_target_root(path);
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cwd)
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

fn session_changes_root(state_root: &Path, project_root: &Path) -> PathBuf {
    state_root
        .join("codex")
        .join("session-changes")
        .join(project_storage_key(project_root))
}

fn legacy_session_changes_root(project_root: &Path) -> PathBuf {
    project_root
        .join(".ai")
        .join("docs")
        .join("session-changes")
}

fn session_bundle_dir(
    state_root: &Path,
    project_root: &Path,
    completed_at_unix: u64,
    session_key: &str,
) -> PathBuf {
    let dt: DateTime<Utc> =
        DateTime::<Utc>::from(UNIX_EPOCH + std::time::Duration::from_secs(completed_at_unix));
    let date_dir = dt.format("%Y-%m-%d").to_string();
    session_changes_root(state_root, project_root)
        .join(date_dir)
        .join(session_key)
}

fn project_storage_key(project_root: &Path) -> String {
    let normalized = normalize_target_root(project_root);
    let slug = normalized
        .file_name()
        .and_then(|value| value.to_str())
        .map(|value| slugify(value, 32))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "project".to_string());
    let hash = blake3::hash(normalized.to_string_lossy().as_bytes()).to_hex();
    format!("{slug}-{}", &hash[..12])
}

fn doc_review_queue_path(state_root: &Path) -> PathBuf {
    state_root.join("codex").join("doc-review-queue.jsonl")
}

fn load_review_queue_for_project(
    queue_path: &Path,
    project_root: &Path,
    limit: usize,
) -> Result<Vec<DocReviewQueueEntry>> {
    if !queue_path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(queue_path)
        .with_context(|| format!("failed to read {}", queue_path.display()))?;
    let project_root = project_root.display().to_string();
    let mut entries = Vec::new();
    for line in contents.lines().rev() {
        if entries.len() >= limit {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<DocReviewQueueEntry>(trimmed) else {
            continue;
        };
        if entry.target_root != project_root {
            continue;
        }
        entries.push(entry);
    }
    entries.reverse();
    Ok(entries)
}

fn path_starts_with_target(candidate: &str, target_root: &Path) -> bool {
    let target = target_root.display().to_string();
    candidate == target || candidate.starts_with(&(target + "/"))
}

fn short_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

fn session_key_from_packet(packet: &SessionDocPacket) -> String {
    format!(
        "{}-{}",
        short_session_id(&packet.session_id),
        packet.completed_at_unix
    )
}

fn unique_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut values = Vec::new();
    for path in paths {
        if seen.insert(path.to_string()) {
            values.push(path.to_string());
        }
    }
    values
}

fn dedupe_preserve_order(values: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for value in values {
        if seen.insert(value.clone()) {
            out.push(value);
        }
    }
    out
}

fn display_repo_relative(target_root: &str, path: &str) -> String {
    let root = Path::new(target_root);
    let candidate = Path::new(path);
    candidate
        .strip_prefix(root)
        .ok()
        .map(|value| value.display().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| path.to_string())
}

fn capitalize_action(action: &str) -> String {
    let mut chars = action.chars();
    let Some(first) = chars.next() else {
        return action.to_string();
    };
    format!("{}{}", first.to_ascii_uppercase(), chars.as_str())
}

fn format_unix_secs(secs: u64) -> String {
    let dt: DateTime<Utc> =
        DateTime::<Utc>::from(UNIX_EPOCH + std::time::Duration::from_secs(secs));
    dt.to_rfc3339()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_git_repo(repo: &Path) {
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo)
            .status()
            .expect("git init");
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(repo)
            .status()
            .expect("git config email");
        Command::new("git")
            .args(["config", "user.name", "Flow Test"])
            .current_dir(repo)
            .status()
            .expect("git config name");
    }

    fn sample_input(
        target_root: &Path,
        patch_changes: Vec<SessionDocPatchChange>,
    ) -> CompletedSessionDocInput {
        let session_file = target_root
            .join(".ai")
            .join("sessions")
            .join("codex")
            .join("session.jsonl");
        fs::create_dir_all(session_file.parent().expect("session dir")).expect("session dir");
        fs::write(&session_file, "").expect("session file");
        CompletedSessionDocInput {
            session_id: "019d035d-99b3-7461-9f15-73306348aa28".to_string(),
            session_file,
            target_path: target_root.display().to_string(),
            launch_path: Some(target_root.display().to_string()),
            first_user_prompt: Some("inspect the current diff".to_string()),
            completion_summary: "implemented the current fix".to_string(),
            completed_at_unix: 1_773_776_290,
            patch_changes,
        }
    }

    #[test]
    fn document_completed_session_writes_packet_index_and_queue() {
        let repo = tempdir().expect("repo");
        let state = tempdir().expect("state");
        unsafe {
            std::env::set_var("FLOW_ACTIVITY_LOG_ROOT", repo.path().join("activity"));
        }

        let packet = document_completed_session_at(
            repo.path(),
            state.path(),
            &sample_input(
                repo.path(),
                vec![SessionDocPatchChange {
                    path: repo.path().join("src/main.rs").display().to_string(),
                    action: "update".to_string(),
                    patch: "@@\n+fn main() {}\n".to_string(),
                }],
            ),
        )
        .expect("packet");

        assert_eq!(packet.confidence, "high");
        assert_eq!(packet.documentation_mode, "packet_with_patch");
        assert!(Path::new(&packet.summary_path).exists());
        assert!(Path::new(&packet.diff_path).exists());
        assert!(Path::new(&packet.summary_path).starts_with(state.path()));
        assert!(Path::new(&packet.diff_path).starts_with(state.path()));

        let recent = recent_packets_at(repo.path(), state.path(), 5).expect("recent");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].session_id, packet.session_id);

        let queue =
            load_review_queue_for_project(&doc_review_queue_path(state.path()), repo.path(), 5)
                .expect("queue");
        assert_eq!(queue.len(), 1);
        assert!(queue[0].commit_eligible);
    }

    #[test]
    fn document_completed_session_marks_metadata_only_without_patch_changes() {
        let repo = tempdir().expect("repo");
        let state = tempdir().expect("state");
        unsafe {
            std::env::set_var("FLOW_ACTIVITY_LOG_ROOT", repo.path().join("activity"));
        }

        let packet = document_completed_session_at(
            repo.path(),
            state.path(),
            &sample_input(repo.path(), Vec::new()),
        )
        .expect("packet");

        assert_eq!(packet.confidence, "low");
        assert_eq!(packet.documentation_mode, "metadata_only");
        assert!(Path::new(&packet.summary_path).starts_with(state.path()));
        let summary = fs::read_to_string(&packet.summary_path).expect("summary");
        assert!(summary.contains("No authoritative patch-attributed file changes were recorded"));
    }

    #[test]
    fn review_queue_entry_records_review_decision_and_target() {
        let repo = tempdir().expect("repo");
        let state = tempdir().expect("state");
        init_git_repo(repo.path());
        unsafe {
            std::env::set_var("FLOW_ACTIVITY_LOG_ROOT", repo.path().join("activity"));
        }

        let packet = document_completed_session_at(
            repo.path(),
            state.path(),
            &sample_input(
                repo.path(),
                vec![SessionDocPatchChange {
                    path: repo.path().join("src/main.rs").display().to_string(),
                    action: "update".to_string(),
                    patch: "@@\n+fn main() {}\n".to_string(),
                }],
            ),
        )
        .expect("packet");

        let queue_path = doc_review_queue_path(state.path());
        let mut queue = load_review_queue_entries(&queue_path).expect("queue");
        review_queue_entry_in_place(state.path(), &mut queue[0]).expect("review");
        assert_eq!(queue[0].doc_review_state, "reviewed");
        assert_eq!(queue[0].promotion_eligible, Some(true));
        assert!(
            queue[0]
                .promotion_target
                .as_deref()
                .unwrap_or_default()
                .contains(".ai/docs/session-changes-promoted/")
        );

        let session_json_path = Path::new(&packet.summary_path)
            .parent()
            .expect("bundle")
            .join("session.json");
        let updated_packet = load_packet(&session_json_path).expect("updated packet");
        assert_eq!(updated_packet.doc_review_state, "reviewed");
        assert!(updated_packet.promotion_target.is_some());
    }

    #[test]
    fn allowed_commit_paths_include_promoted_note_and_metadata() {
        let repo = tempdir().expect("repo");
        let state = tempdir().expect("state");
        let session_root = session_changes_root(state.path(), repo.path())
            .join("2026-03-19")
            .join("019d");
        let entry = DocReviewQueueEntry {
            version: DOC_REVIEW_QUEUE_VERSION,
            enqueued_at_unix: 1,
            session_id: "019d035d-99b3-7461-9f15-73306348aa28".to_string(),
            session_key: "019d035d-1773776290".to_string(),
            target_root: repo.path().display().to_string(),
            storage_class: "project_ai_docs".to_string(),
            confidence: "high".to_string(),
            documentation_mode: "packet_with_patch".to_string(),
            changed_files: vec!["src/main.rs".to_string()],
            trace_id: None,
            summary: "implemented the current fix".to_string(),
            session_json_path: session_root.join("session.json").display().to_string(),
            summary_path: session_root.join("summary.md").display().to_string(),
            diff_path: session_root.join("diff.txt").display().to_string(),
            promotion_eligible: Some(true),
            promotion_target: Some(".ai/docs/session-changes-promoted/demo.md".to_string()),
            promotion_reason: Some("promote".to_string()),
            review_decision: Some("reviewed".to_string()),
            commit_eligible: true,
            commit_decision: Some("pending".to_string()),
            blocked_reason: None,
            reviewed_at_unix: Some(1),
            promoted_paths: vec![
                repo.path()
                    .join(".ai/docs/session-changes-promoted/demo.md")
                    .display()
                    .to_string(),
            ],
            doc_review_state: "promoted".to_string(),
        };

        let paths = allowed_commit_paths(repo.path(), &entry);
        assert!(
            paths
                .iter()
                .any(|path| path == ".ai/docs/session-changes-promoted/demo.md")
        );
        assert!(paths.iter().any(|path| path.ends_with("summary.md")));
        assert!(paths.iter().any(|path| path.ends_with("promotion.json")));
    }

    #[test]
    fn recent_packets_falls_back_to_legacy_project_storage() {
        let repo = tempdir().expect("repo");
        let state = tempdir().expect("state");
        let legacy_root = legacy_session_changes_root(repo.path());
        fs::create_dir_all(&legacy_root).expect("legacy root");
        let index_path = legacy_root.join("index.json");
        let index = SessionDocIndex {
            version: SESSION_DOC_INDEX_VERSION,
            generated_at_unix: 1,
            total_sessions: 1,
            recent_sessions: vec![SessionDocIndexEntry {
                session_id: "019d035d-99b3-7461-9f15-73306348aa28".to_string(),
                session_key: "019d035d-1773776290".to_string(),
                completed_at_unix: 1_773_776_290,
                confidence: "high".to_string(),
                documentation_mode: "packet_with_patch".to_string(),
                changed_files: vec!["src/main.rs".to_string()],
                completion_summary: "implemented the current fix".to_string(),
                summary_path: legacy_root
                    .join("2026-03-19/019d/summary.md")
                    .display()
                    .to_string(),
                diff_path: legacy_root
                    .join("2026-03-19/019d/diff.txt")
                    .display()
                    .to_string(),
                session_json_path: legacy_root
                    .join("2026-03-19/019d/session.json")
                    .display()
                    .to_string(),
            }],
            by_file: BTreeMap::new(),
        };
        fs::write(
            &index_path,
            serde_json::to_vec_pretty(&index).expect("encode index"),
        )
        .expect("write index");

        let recent = recent_packets_at(repo.path(), state.path(), 5).expect("recent");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].session_key, "019d035d-1773776290");
    }
}
