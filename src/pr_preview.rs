use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::cli::{PrOpts, PrPreviewModeArg};

pub const FLOW_PR_PREVIEW_JSON_FILENAME: &str = "flow-pr-preview.json";
pub const FLOW_PR_PREVIEW_MD_FILENAME: &str = "flow-pr-preview.md";
pub const FLOW_PR_CREATE_FILENAME: &str = "flow-pr-create.json";
pub const FLOW_REVIEW_PREVIEW_STATE_FILENAME: &str = "flow-review-preview-state.json";
pub const FLOW_REVIEW_PREVIEW_CHECK_FILENAME: &str = "flow-review-preview-check.json";
pub const FLOW_REVIEW_PREVIEW_KIT_JSON_FILENAME: &str = "flow-review-preview-kit.json";
pub const FLOW_REVIEW_PREVIEW_KIT_LOG_FILENAME: &str = "flow-review-preview-kit.log";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PrPreviewStatus {
    Clean,
    Warning,
    Blocked,
}

impl PrPreviewStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Warning => "warning",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PrPreviewCommand {
    pub repo_path: Option<PathBuf>,
    pub requested_base: String,
    pub mode: PrPreviewModeArg,
    pub json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrPreviewDiffStats {
    pub files: usize,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrPreviewChangedFile {
    pub path: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    pub category: String,
    pub risky: bool,
    pub ui: bool,
    pub test: bool,
    pub docs: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrPreviewRiskyFile {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrPreviewValidationSignal {
    pub name: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrPreviewScopeLeak {
    pub category: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrPreviewIssue {
    pub external_ref: String,
    pub severity: String,
    pub rule: String,
    pub title: String,
    pub summary: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub diff_hunk: Option<String>,
    pub evidence: Vec<String>,
    pub fix_hint: String,
    pub validation_hint: String,
    pub prevention_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrPreviewPacket {
    pub version: u32,
    pub kind: String,
    pub mode: String,
    pub repo: String,
    pub repo_root: String,
    pub generated_at: String,
    pub requested_base_ref: String,
    pub resolved_base_ref: String,
    pub merge_base_ref: String,
    pub compare_label: String,
    pub head_ref: String,
    pub head_sha: String,
    pub pr_title: String,
    pub generated_title: String,
    pub generated_body: String,
    pub ready_for_draft: bool,
    pub preview_status: String,
    pub blockers_count: usize,
    pub warnings_count: usize,
    pub diff_stats: PrPreviewDiffStats,
    pub changed_files: Vec<PrPreviewChangedFile>,
    pub risky_files: Vec<PrPreviewRiskyFile>,
    pub validation_results: Vec<PrPreviewValidationSignal>,
    pub scope_leaks: Vec<PrPreviewScopeLeak>,
    pub blockers: Vec<PrPreviewIssue>,
    pub warnings: Vec<PrPreviewIssue>,
    pub items: Vec<PrPreviewSnapshotItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrPreviewSnapshotItem {
    pub external_ref: String,
    pub source: String,
    pub author: String,
    pub body: String,
    pub url: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub diff_hunk: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrPreviewReviewState {
    target_dir: String,
    review_root: String,
    review_plan: String,
    snapshot_json: String,
    kit_system: String,
    kit_json: String,
    kit_log: String,
    kit_status: String,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrPreviewCheckState {
    version: u32,
    review_plan: String,
    snapshot_json: String,
    current_index: usize,
    items: Vec<PrPreviewCheckItem>,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrPreviewCheckItem {
    external_ref: String,
    concern_status: String,
    verdict: String,
    why: String,
    fix: String,
    validation: String,
    prevention: String,
    coach_findings: String,
    kit_upgrade: String,
    ledger_update: String,
    patched: bool,
    validated: bool,
    prevention_captured: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrCreateDraftPacket {
    pub version: u32,
    pub mode: String,
    pub repo: String,
    pub repo_root: String,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub title: String,
    pub body: String,
    pub ready_for_draft: bool,
    pub preview_status: String,
    pub blockers_count: usize,
    pub warnings_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrPreviewRunResult {
    pub status: String,
    pub source: String,
    pub review_root: String,
    pub preview_json_path: String,
    pub review_plan_path: String,
    pub pr_create_path: String,
    pub compare_label: String,
    pub item_count: usize,
    pub ready_for_draft: bool,
    pub blockers_count: usize,
    pub warnings_count: usize,
    pub preview_status: String,
}

#[derive(Debug, Clone)]
struct ResolvedBase {
    requested: String,
    resolved: String,
    merge_base: String,
    compare_label: String,
    fallback_used: bool,
}

#[derive(Debug, Clone)]
struct IssueTemplate {
    severity: &'static str,
    rule: &'static str,
    title: String,
    summary: String,
    path: Option<String>,
    evidence: Vec<String>,
    fix_hint: String,
    validation_hint: String,
    prevention_hint: String,
}

#[derive(Debug, Clone)]
struct PreviewContext {
    repo_root: PathBuf,
    review_root: PathBuf,
    head_ref: String,
    head_label: String,
    compare_head_label: String,
    work_tree_root: Option<PathBuf>,
}

pub fn parse_pr_preview_args(args: &[String], opts: &PrOpts) -> Result<Option<PrPreviewCommand>> {
    if args.first().map(|value| value.as_str()) != Some("preview") {
        return Ok(None);
    }

    let mut repo_path = match opts.paths.len() {
        0 => None,
        1 => Some(PathBuf::from(&opts.paths[0])),
        _ => bail!("`f pr preview --path` accepts at most one repo path override"),
    };
    let mut requested_base = opts.base.clone();
    let mut mode = opts.mode.unwrap_or(PrPreviewModeArg::Draft);
    let mut json = opts.json;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => {
                json = true;
                index += 1;
            }
            "--mode" => {
                let Some(value) = args.get(index + 1) else {
                    bail!("`f pr preview --mode` requires a value");
                };
                mode = parse_mode_arg(value)?;
                index += 2;
            }
            "--base" => {
                let Some(value) = args.get(index + 1) else {
                    bail!("`f pr preview --base` requires a value");
                };
                requested_base = value.clone();
                index += 2;
            }
            "--path" => {
                let Some(value) = args.get(index + 1) else {
                    bail!("`f pr preview --path` requires a value");
                };
                repo_path = Some(PathBuf::from(value));
                index += 2;
            }
            token => bail!("unknown `f pr preview` option: {token}"),
        }
    }

    Ok(Some(PrPreviewCommand {
        repo_path,
        requested_base,
        mode,
        json,
    }))
}

fn parse_mode_arg(value: &str) -> Result<PrPreviewModeArg> {
    match value.trim().to_ascii_lowercase().as_str() {
        "draft" => Ok(PrPreviewModeArg::Draft),
        "feedback" => Ok(PrPreviewModeArg::Feedback),
        other => bail!("unsupported `f pr preview --mode` value: {other}"),
    }
}

pub fn run_pr_preview(cmd: PrPreviewCommand) -> Result<()> {
    let start = cmd
        .repo_path
        .clone()
        .unwrap_or(std::env::current_dir().context("failed to resolve current directory")?);
    let context = resolve_preview_context(&start)?;
    let packet = build_preview_packet(&context, &cmd)?;
    let result = if packet.diff_stats.files == 0 {
        clear_preview_artifacts(&context.review_root)?;
        PrPreviewRunResult {
            status: "cleared".to_string(),
            source: "flow".to_string(),
            review_root: context.review_root.display().to_string(),
            preview_json_path: context
                .review_root
                .join(".ai/reviews")
                .join(FLOW_PR_PREVIEW_JSON_FILENAME)
                .display()
                .to_string(),
            review_plan_path: context
                .review_root
                .join(".ai/reviews")
                .join(FLOW_PR_PREVIEW_MD_FILENAME)
                .display()
                .to_string(),
            pr_create_path: context
                .review_root
                .join(".ai/reviews")
                .join(FLOW_PR_CREATE_FILENAME)
                .display()
                .to_string(),
            compare_label: packet.compare_label.clone(),
            item_count: 0,
            ready_for_draft: false,
            blockers_count: 0,
            warnings_count: 0,
            preview_status: PrPreviewStatus::Blocked.as_str().to_string(),
        }
    } else {
        let result = write_preview_artifacts(&context.review_root, &packet)?;
        result
    };

    if cmd.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("PR preview: {}", result.preview_status);
        println!("Compare: {}", result.compare_label);
        println!(
            "Items: {}  Blockers: {}  Warnings: {}",
            result.item_count, result.blockers_count, result.warnings_count
        );
        println!(
            "Ready for draft: {}",
            if result.ready_for_draft { "yes" } else { "no" }
        );
        println!("Preview JSON: {}", result.preview_json_path);
        println!("Preview plan: {}", result.review_plan_path);
        println!("Draft payload: {}", result.pr_create_path);
    }

    Ok(())
}

fn build_preview_packet(
    context: &PreviewContext,
    cmd: &PrPreviewCommand,
) -> Result<PrPreviewPacket> {
    let generated_at = chrono::Utc::now().to_rfc3339();
    let repo = detect_repo_label(&context.repo_root)?;
    let head_ref = context.head_label.clone();
    let head_sha = git_rev_parse(&context.repo_root, &context.head_ref)?;
    let base = resolve_compare_base(
        &context.repo_root,
        &context.head_ref,
        &context.compare_head_label,
        &cmd.requested_base,
    )?;
    let changed_files = collect_changed_files(
        &context.repo_root,
        context.work_tree_root.as_deref(),
        &base.merge_base,
    )?;
    let diff_stats = PrPreviewDiffStats {
        files: changed_files.len(),
        additions: changed_files.iter().map(|item| item.additions).sum(),
        deletions: changed_files.iter().map(|item| item.deletions).sum(),
    };
    let risky_files = collect_risky_files(&changed_files);
    let scope_leaks = collect_scope_leaks(&changed_files);
    let validation_results = collect_validation_results(&changed_files);
    let (generated_title, generated_body) = build_pr_copy(
        &head_ref,
        &base.compare_label,
        &changed_files,
        &validation_results,
    );
    let mut issues = collect_preview_issues(
        &context.repo_root,
        context.work_tree_root.as_deref(),
        &head_ref,
        &base,
        &changed_files,
        &diff_stats,
        &risky_files,
        &scope_leaks,
        &validation_results,
        &generated_title,
        &generated_body,
    )?;
    if issues.is_empty() && diff_stats.files > 0 {
        issues.push(build_ready_issue(&generated_title, &base.compare_label));
    }
    let blockers: Vec<PrPreviewIssue> = issues
        .iter()
        .filter(|issue| issue.severity == "blocker")
        .cloned()
        .collect();
    let warnings: Vec<PrPreviewIssue> = issues
        .iter()
        .filter(|issue| issue.severity == "warning")
        .cloned()
        .collect();
    let preview_status = if !blockers.is_empty() {
        PrPreviewStatus::Blocked
    } else if !warnings.is_empty() {
        PrPreviewStatus::Warning
    } else {
        PrPreviewStatus::Clean
    };
    let items = issues
        .into_iter()
        .map(|issue| PrPreviewSnapshotItem {
            external_ref: issue.external_ref.clone(),
            source: format!("preview:{}", issue.severity),
            author: "Flow".to_string(),
            body: render_issue_body(&issue),
            url: String::new(),
            path: issue.path.clone(),
            line: issue.line,
            diff_hunk: issue.diff_hunk.clone(),
        })
        .collect();

    Ok(PrPreviewPacket {
        version: 1,
        kind: "local-preview".to_string(),
        mode: match cmd.mode {
            PrPreviewModeArg::Draft => "draft",
            PrPreviewModeArg::Feedback => "feedback",
        }
        .to_string(),
        repo,
        repo_root: context.repo_root.display().to_string(),
        generated_at,
        requested_base_ref: base.requested,
        resolved_base_ref: base.resolved,
        merge_base_ref: base.merge_base,
        compare_label: base.compare_label,
        head_ref,
        head_sha,
        pr_title: generated_title.clone(),
        generated_title,
        generated_body,
        ready_for_draft: preview_status != PrPreviewStatus::Blocked,
        preview_status: preview_status.as_str().to_string(),
        blockers_count: blockers.len(),
        warnings_count: warnings.len(),
        diff_stats,
        changed_files,
        risky_files,
        validation_results,
        scope_leaks,
        blockers,
        warnings,
        items,
    })
}

fn write_preview_artifacts(
    review_root: &Path,
    packet: &PrPreviewPacket,
) -> Result<PrPreviewRunResult> {
    let reviews_dir = review_root.join(".ai").join("reviews");
    fs::create_dir_all(&reviews_dir)
        .with_context(|| format!("failed to create {}", reviews_dir.display()))?;

    let preview_json_path = reviews_dir.join(FLOW_PR_PREVIEW_JSON_FILENAME);
    let review_plan_path = reviews_dir.join(FLOW_PR_PREVIEW_MD_FILENAME);
    let pr_create_path = reviews_dir.join(FLOW_PR_CREATE_FILENAME);
    let review_state_path = reviews_dir.join(FLOW_REVIEW_PREVIEW_STATE_FILENAME);
    let review_check_path = reviews_dir.join(FLOW_REVIEW_PREVIEW_CHECK_FILENAME);
    let previous_check = read_json::<PrPreviewCheckState>(&review_check_path).ok();
    let previous_items: HashMap<String, PrPreviewCheckItem> = previous_check
        .as_ref()
        .map(|state| {
            state
                .items
                .iter()
                .map(|item| (item.external_ref.clone(), item.clone()))
                .collect()
        })
        .unwrap_or_default();
    let check_items: Vec<PrPreviewCheckItem> = packet
        .items
        .iter()
        .map(|item| {
            previous_items
                .get(&item.external_ref)
                .cloned()
                .unwrap_or_else(|| empty_check_item(&item.external_ref))
        })
        .collect();
    let review_state = PrPreviewReviewState {
        target_dir: review_root.display().to_string(),
        review_root: review_root.display().to_string(),
        review_plan: review_plan_path.display().to_string(),
        snapshot_json: preview_json_path.display().to_string(),
        kit_system: String::new(),
        kit_json: reviews_dir
            .join(FLOW_REVIEW_PREVIEW_KIT_JSON_FILENAME)
            .display()
            .to_string(),
        kit_log: reviews_dir
            .join(FLOW_REVIEW_PREVIEW_KIT_LOG_FILENAME)
            .display()
            .to_string(),
        kit_status: "not-run".to_string(),
        updated_at: packet.generated_at.clone(),
    };
    let review_check = PrPreviewCheckState {
        version: 1,
        review_plan: review_plan_path.display().to_string(),
        snapshot_json: preview_json_path.display().to_string(),
        current_index: previous_check
            .as_ref()
            .map(|state| {
                state
                    .current_index
                    .min(packet.items.len().saturating_sub(1))
            })
            .unwrap_or(0),
        items: check_items,
        updated_at: packet.generated_at.clone(),
    };
    let create_packet = PrCreateDraftPacket {
        version: 1,
        mode: packet.mode.clone(),
        repo: packet.repo.clone(),
        repo_root: packet.repo_root.clone(),
        head_ref: packet.head_ref.clone(),
        head_sha: packet.head_sha.clone(),
        base_ref: packet.resolved_base_ref.clone(),
        title: packet.generated_title.clone(),
        body: packet.generated_body.clone(),
        ready_for_draft: packet.ready_for_draft,
        preview_status: packet.preview_status.clone(),
        blockers_count: packet.blockers_count,
        warnings_count: packet.warnings_count,
    };

    write_json(&preview_json_path, packet)?;
    fs::write(&review_plan_path, build_preview_markdown(packet))
        .with_context(|| format!("failed to write {}", review_plan_path.display()))?;
    write_json(&review_state_path, &review_state)?;
    write_json(&review_check_path, &review_check)?;
    write_json(&pr_create_path, &create_packet)?;

    Ok(PrPreviewRunResult {
        status: "created".to_string(),
        source: "flow".to_string(),
        review_root: review_root.display().to_string(),
        preview_json_path: preview_json_path.display().to_string(),
        review_plan_path: review_plan_path.display().to_string(),
        pr_create_path: pr_create_path.display().to_string(),
        compare_label: packet.compare_label.clone(),
        item_count: packet.items.len(),
        ready_for_draft: packet.ready_for_draft,
        blockers_count: packet.blockers_count,
        warnings_count: packet.warnings_count,
        preview_status: packet.preview_status.clone(),
    })
}

fn clear_preview_artifacts(review_root: &Path) -> Result<()> {
    let reviews_dir = review_root.join(".ai").join("reviews");
    let files = [
        reviews_dir.join(FLOW_PR_PREVIEW_JSON_FILENAME),
        reviews_dir.join(FLOW_PR_PREVIEW_MD_FILENAME),
        reviews_dir.join(FLOW_PR_CREATE_FILENAME),
        reviews_dir.join(FLOW_REVIEW_PREVIEW_STATE_FILENAME),
        reviews_dir.join(FLOW_REVIEW_PREVIEW_CHECK_FILENAME),
        reviews_dir.join(FLOW_REVIEW_PREVIEW_KIT_JSON_FILENAME),
        reviews_dir.join(FLOW_REVIEW_PREVIEW_KIT_LOG_FILENAME),
        reviews_dir.join("flow-review-preview-snapshot.json"),
        reviews_dir.join("flow-review-preview-plan.md"),
    ];
    for file in files {
        match fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to remove {}", file.display()));
            }
        }
    }
    Ok(())
}

fn build_preview_markdown(packet: &PrPreviewPacket) -> String {
    let mut out = String::new();
    out.push_str("# PR Preview\n\n");
    out.push_str(&format!("- Repo: `{}`\n", packet.repo));
    out.push_str(&format!("- Head: `{}`\n", packet.head_ref));
    out.push_str(&format!("- Compare: `{}`\n", packet.compare_label));
    out.push_str(&format!("- Status: `{}`\n", packet.preview_status));
    out.push_str(&format!(
        "- Ready for draft: `{}`\n",
        if packet.ready_for_draft { "yes" } else { "no" }
    ));
    out.push_str(&format!(
        "- Diff: `{}` files, `+{}` / `-{}` lines\n\n",
        packet.diff_stats.files, packet.diff_stats.additions, packet.diff_stats.deletions
    ));
    out.push_str("## Draft PR\n\n");
    out.push_str(&format!("### Title\n\n{}\n\n", packet.generated_title));
    out.push_str("### Body\n\n");
    out.push_str("```md\n");
    out.push_str(packet.generated_body.trim());
    out.push_str("\n```\n\n");
    out.push_str("## Validation Signals\n\n");
    for signal in &packet.validation_results {
        out.push_str(&format!(
            "- `{}`: {}. {}\n",
            signal.name, signal.status, signal.detail
        ));
    }
    out.push('\n');

    for (index, item) in packet.items.iter().enumerate() {
        let label = item
            .path
            .as_deref()
            .map(|path| {
                if let Some(line) = item.line {
                    format!("{path}:{line}")
                } else {
                    path.to_string()
                }
            })
            .unwrap_or_else(|| item.external_ref.clone());
        out.push_str(&format!("## Item {}: {}\n\n", index + 1, label));
        out.push_str("### Reviewer Feedback\n\n");
        out.push_str(&format!("- Source: `{}`\n\n", item.source));
        out.push_str(item.body.trim());
        out.push_str("\n\n### Concern Status\n\n");
        out.push_str("- choose one: still applies here / moved nearby / already resolved / not a real issue\n");
        out.push_str("- decide whether the preview concern still matters for this diff\n\n");
        out.push_str("### Local Verdict\n\n");
        out.push_str("- explain whether the gate is correct, partially correct, or noisy\n");
        out.push_str("- name the real scope or validation gap before changing code\n\n");
        out.push_str("### Narrow Fix\n\n");
        out.push_str("- describe the smallest acceptable change to get this PR ready\n");
        out.push_str("- keep unrelated cleanup out of the patch\n\n");
        out.push_str("### Validation\n\n");
        out.push_str("- name the exact command or manual check that clears this concern\n");
        out.push_str("- confirm the intended behavior and the scope both hold\n\n");
        out.push_str("### Prevention Candidate\n\n");
        out.push_str(
            "- suggest a durable rule, test, lint, or review check if this is repeatable\n",
        );
        out.push_str("- otherwise say explicitly that no durable prevention is warranted\n\n");
        out.push_str("### Status\n\n");
        out.push_str("- [x] open\n");
        out.push_str("- [ ] patched\n");
        out.push_str("- [ ] validated\n");
        out.push_str("- [ ] prevention-captured\n\n");
    }

    out
}

fn collect_preview_issues(
    repo_root: &Path,
    work_tree_root: Option<&Path>,
    head_ref: &str,
    base: &ResolvedBase,
    changed_files: &[PrPreviewChangedFile],
    diff_stats: &PrPreviewDiffStats,
    risky_files: &[PrPreviewRiskyFile],
    scope_leaks: &[PrPreviewScopeLeak],
    validation_results: &[PrPreviewValidationSignal],
    generated_title: &str,
    generated_body: &str,
) -> Result<Vec<PrPreviewIssue>> {
    let mut templates = Vec::new();

    if head_ref.is_empty() || head_ref == "HEAD" || matches!(head_ref, "main" | "master" | "trunk")
    {
        templates.push(IssueTemplate {
            severity: "blocker",
            rule: "wrong-branch",
            title: "Preview is running from the base lane, not a review branch".to_string(),
            summary: format!("Current head is `{head_ref}`. Draft PRs should not be cut directly from the home/base branch."),
            path: None,
            evidence: vec![
                format!("Head ref: `{head_ref}`"),
                "Create or switch to a scoped review/work branch first.".to_string(),
            ],
            fix_hint: "Move the change onto a dedicated branch before creating or updating a PR.".to_string(),
            validation_hint: "Re-run `f pr preview` from the review branch and confirm the head/base pairing looks correct.".to_string(),
            prevention_hint: "Keep the home lane clean and use preview workspaces for publishable diffs.".to_string(),
        });
    }

    if base.fallback_used {
        templates.push(IssueTemplate {
            severity: "warning",
            rule: "base-fallback",
            title: "Preview fell back to a weaker diff base".to_string(),
            summary: format!("Flow used `{}` because the requested base could not be resolved cleanly.", base.compare_label),
            path: None,
            evidence: vec![
                format!("Requested base: `{}`", base.requested),
                format!("Resolved compare: `{}`", base.compare_label),
            ],
            fix_hint: "Pass an explicit `--base` or repair the remote/base branch before publishing.".to_string(),
            validation_hint: "Inspect the changed file list and confirm the diff is scoped to the intended review branch.".to_string(),
            prevention_hint: "Keep origin/main and the local home branch in sync so preview can resolve the correct merge-base automatically.".to_string(),
        });
    }

    let leaking_paths: Vec<String> = changed_files
        .iter()
        .filter(|file| is_internal_overlay_path(&file.path))
        .map(|file| file.path.clone())
        .collect();
    if !leaking_paths.is_empty() {
        templates.push(IssueTemplate {
            severity: "blocker",
            rule: "scope-leak",
            title: "Local-only workflow files leaked into the PR diff".to_string(),
            summary: "Preview found files that normally should stay out of publishable review diffs.".to_string(),
            path: leaking_paths.first().cloned(),
            evidence: leaking_paths
                .iter()
                .map(|path| format!("Leaked path: `{path}`"))
                .collect(),
            fix_hint: "Drop local-only paths from the branch or split them into a private lane before creating the PR.".to_string(),
            validation_hint: "Re-run preview and confirm the changed file list no longer contains personal/workflow artifacts.".to_string(),
            prevention_hint: "Keep review branches scoped to publishable product code and use repo-local ignore/personal overlays sparingly.".to_string(),
        });
    }

    if diff_stats.files > 24 || diff_stats.additions + diff_stats.deletions > 1200 {
        templates.push(IssueTemplate {
            severity: "blocker",
            rule: "oversized-pr",
            title: "The proposed PR is too large to review safely".to_string(),
            summary: format!(
                "Preview sees {} files and {} changed lines, which is outside the safe draft-PR envelope.",
                diff_stats.files,
                diff_stats.additions + diff_stats.deletions
            ),
            path: None,
            evidence: vec![
                format!("Files changed: {}", diff_stats.files),
                format!("Lines changed: {}", diff_stats.additions + diff_stats.deletions),
            ],
            fix_hint: "Split the work into smaller PRs before sending it for review.".to_string(),
            validation_hint: "Re-run preview after splitting and confirm each PR has a tighter file/line count.".to_string(),
            prevention_hint: "Use stacked branches and pre-PR preview earlier so scope stays small.".to_string(),
        });
    } else if diff_stats.files > 12 || diff_stats.additions + diff_stats.deletions > 400 {
        templates.push(IssueTemplate {
            severity: "warning",
            rule: "oversized-pr",
            title: "The diff is approaching the review-size limit".to_string(),
            summary: format!(
                "Preview sees {} files and {} changed lines. The PR may still be publishable, but reviewers will have a harder time holding the full context.",
                diff_stats.files,
                diff_stats.additions + diff_stats.deletions
            ),
            path: None,
            evidence: vec![
                format!("Files changed: {}", diff_stats.files),
                format!("Lines changed: {}", diff_stats.additions + diff_stats.deletions),
            ],
            fix_hint: "Consider splitting unrelated follow-up cleanup or support work into a separate PR.".to_string(),
            validation_hint: "Confirm the PR description explains the single concern clearly if you keep this scope.".to_string(),
            prevention_hint: "Preview branches before the last-mile cleanup so extra concerns do not accumulate.".to_string(),
        });
    }

    if !scope_leaks.is_empty() {
        templates.push(IssueTemplate {
            severity: "warning",
            rule: "mixed-concerns",
            title: "The diff spans multiple concern areas".to_string(),
            summary: "Changed files cross more than one major product/tooling area, which can make the PR hard to explain and validate.".to_string(),
            path: None,
            evidence: scope_leaks
                .iter()
                .map(|item| format!("{}: {}", item.category, item.detail))
                .collect(),
            fix_hint: "Split unrelated areas or tighten the PR body so reviewers understand why these files must move together.".to_string(),
            validation_hint: "Check that the draft title/body describe one coherent change rather than a grab bag.".to_string(),
            prevention_hint: "Use separate leaves for product work, tooling work, and docs-only follow-ups.".to_string(),
        });
    }

    let missing_validation = validation_results
        .iter()
        .find(|item| item.name == "validation");
    if let Some(signal) = missing_validation.filter(|item| item.status == "warning") {
        templates.push(IssueTemplate {
            severity: "warning",
            rule: "missing-validation",
            title: "The diff changes product code without obvious validation coverage".to_string(),
            summary: signal.detail.clone(),
            path: changed_files
                .iter()
                .find(|file| !file.docs && !file.test)
                .map(|file| file.path.clone()),
            evidence: changed_files
                .iter()
                .filter(|file| !file.docs)
                .take(5)
                .map(|file| format!("Changed path: `{}`", file.path))
                .collect(),
            fix_hint: "Add the smallest relevant automated or manual validation note before creating the draft PR.".to_string(),
            validation_hint: "List the exact command or manual repro you ran in the draft body or review item state.".to_string(),
            prevention_hint: "Default to adding validation evidence at preview time so the draft PR body is never empty on proof.".to_string(),
        });
    }

    let ui_signal = validation_results
        .iter()
        .find(|item| item.name == "ui-evidence");
    if let Some(signal) = ui_signal.filter(|item| item.status == "warning") {
        templates.push(IssueTemplate {
            severity: "warning",
            rule: "ui-evidence",
            title: "UI changes are missing screenshot or operator note evidence".to_string(),
            summary: signal.detail.clone(),
            path: changed_files.iter().find(|file| file.ui).map(|file| file.path.clone()),
            evidence: changed_files
                .iter()
                .filter(|file| file.ui)
                .map(|file| format!("UI path: `{}`", file.path))
                .collect(),
            fix_hint: "Add a screenshot, screencast note, or operator-facing validation note before drafting the PR.".to_string(),
            validation_hint: "Capture the exact view/state you validated and include it in the PR summary.".to_string(),
            prevention_hint: "Treat UI diffs as incomplete until visual evidence is attached.".to_string(),
        });
    }

    if !risky_files.is_empty() {
        templates.push(IssueTemplate {
            severity: if risky_files.iter().any(|item| item.reason.contains("without")) {
                "blocker"
            } else {
                "warning"
            },
            rule: "generated-noise",
            title: "Generated or high-churn files need an intent check".to_string(),
            summary: "Preview found lockfiles, generated artifacts, or heavy workflow files that often leak into PRs unintentionally.".to_string(),
            path: risky_files.first().map(|item| item.path.clone()),
            evidence: risky_files
                .iter()
                .map(|item| format!("{}: {}", item.path, item.reason))
                .collect(),
            fix_hint: "Keep only the risky files that are required for the intended change and explain them in the PR body.".to_string(),
            validation_hint: "Confirm each risky file has a corresponding source or manifest change that justifies it.".to_string(),
            prevention_hint: "Make preview a required step before pushing lockfiles or generated output.".to_string(),
        });
    }

    if pr_copy_is_risky(generated_title, generated_body) {
        templates.push(IssueTemplate {
            severity: "warning",
            rule: "pr-copy",
            title: "The generated PR copy is still too generic".to_string(),
            summary: "Draft PR text should explain scope, validation, and risk without hand-wavy wording.".to_string(),
            path: None,
            evidence: vec![
                format!("Generated title: {}", generated_title),
                "Inspect the generated body sections for specificity before publishing.".to_string(),
            ],
            fix_hint: "Rewrite the title/body so the Summary and Validation sections say exactly what changed and how it was checked.".to_string(),
            validation_hint: "Read the body once as a reviewer and confirm it answers why, scope, and proof.".to_string(),
            prevention_hint: "Keep branch names and preview summaries specific so generated copy starts from real nouns instead of placeholders.".to_string(),
        });
    }

    let durable_lesson_paths: Vec<&PrPreviewChangedFile> = changed_files
        .iter()
        .filter(|file| touches_workflow_or_tooling(&file.path))
        .collect();
    let docs_changed = changed_files.iter().any(|file| file.docs);
    if !durable_lesson_paths.is_empty() && !docs_changed {
        templates.push(IssueTemplate {
            severity: "warning",
            rule: "durable-lesson",
            title: "Tooling/workflow changes may need a durable lesson".to_string(),
            summary: "This diff touches review or workflow surfaces, but preview did not see any docs/rules note that explains the new contract.".to_string(),
            path: durable_lesson_paths.first().map(|file| file.path.clone()),
            evidence: durable_lesson_paths
                .iter()
                .map(|file| format!("Workflow path: `{}`", file.path))
                .collect(),
            fix_hint: "If this change closes a repeated failure mode, capture the lesson in docs or review rules before publishing.".to_string(),
            validation_hint: "Name the operator-visible rule or workflow improvement that this PR is meant to establish.".to_string(),
            prevention_hint: "Route durable workflow lessons into docs/Kit while the reasoning is still fresh.".to_string(),
        });
    }

    templates
        .into_iter()
        .map(|template| enrich_issue(repo_root, work_tree_root, base, template))
        .collect()
}

fn build_ready_issue(generated_title: &str, compare_label: &str) -> PrPreviewIssue {
    let summary = format!(
        "Preview did not find blockers. The diff is ready for a draft PR once you confirm the title/body and validation text."
    );
    let mut hasher = Sha1::new();
    hasher.update(summary.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    PrPreviewIssue {
        external_ref: format!("preview:ready:{}", &digest[..12]),
        severity: "info".to_string(),
        rule: "ready-for-draft".to_string(),
        title: "Preview looks ready for a draft PR".to_string(),
        summary,
        path: None,
        line: None,
        diff_hunk: None,
        evidence: vec![
            format!("Generated title: {}", generated_title),
            format!("Compare base: {}", compare_label),
        ],
        fix_hint: "Keep the diff scoped and open the draft PR from this preview state.".to_string(),
        validation_hint: "Copy the validation notes into the draft PR body before publishing.".to_string(),
        prevention_hint: "Repeat the preview loop before every publish step so the draft stays reviewer-friendly.".to_string(),
    }
}

fn collect_validation_results(files: &[PrPreviewChangedFile]) -> Vec<PrPreviewValidationSignal> {
    let mut signals = Vec::new();
    let product_paths: Vec<&PrPreviewChangedFile> = files
        .iter()
        .filter(|file| {
            !file.docs
                && !file.test
                && !is_manifest_path(&file.path)
                && !is_lockfile_path(&file.path)
        })
        .collect();
    let tests_changed = files.iter().any(|file| file.test);
    let ui_changed = files.iter().any(|file| file.ui);
    let visual_note = files
        .iter()
        .any(|file| file.docs || is_visual_artifact_path(&file.path));

    signals.push(PrPreviewValidationSignal {
        name: "validation".to_string(),
        status: if product_paths.is_empty() || tests_changed {
            "pass".to_string()
        } else {
            "warning".to_string()
        },
        detail: if product_paths.is_empty() {
            "No product paths changed, so validation burden is low.".to_string()
        } else if tests_changed {
            "Preview found matching test or validation paths in the diff.".to_string()
        } else {
            "Product-facing files changed without an obvious test or validation artifact in the diff.".to_string()
        },
    });

    signals.push(PrPreviewValidationSignal {
        name: "ui-evidence".to_string(),
        status: if !ui_changed || visual_note {
            "pass".to_string()
        } else {
            "warning".to_string()
        },
        detail: if !ui_changed {
            "No UI-facing files changed.".to_string()
        } else if visual_note {
            "Preview found docs or visual artifacts that can carry UI validation evidence."
                .to_string()
        } else {
            "UI-facing files changed without any screenshot, note, or docs artifact in the diff."
                .to_string()
        },
    });

    signals
}

fn collect_scope_leaks(files: &[PrPreviewChangedFile]) -> Vec<PrPreviewScopeLeak> {
    let mut categories: HashMap<String, usize> = HashMap::new();
    for file in files {
        if file.docs || file.test {
            continue;
        }
        *categories.entry(file.category.clone()).or_insert(0) += 1;
    }
    if categories.len() <= 2 {
        return Vec::new();
    }
    let mut leaks = categories
        .into_iter()
        .map(|(category, count)| PrPreviewScopeLeak {
            category,
            detail: format!("{count} file(s) changed in this area"),
        })
        .collect::<Vec<_>>();
    leaks.sort_by(|left, right| left.category.cmp(&right.category));
    leaks
}

fn collect_risky_files(files: &[PrPreviewChangedFile]) -> Vec<PrPreviewRiskyFile> {
    let changed_paths: HashSet<&str> = files.iter().map(|file| file.path.as_str()).collect();
    let mut risky = Vec::new();
    for file in files {
        if is_lockfile_path(&file.path) && !matching_manifest_changed(&file.path, &changed_paths) {
            risky.push(PrPreviewRiskyFile {
                path: file.path.clone(),
                reason: "lockfile changed without the corresponding manifest".to_string(),
            });
        } else if is_generated_artifact_path(&file.path)
            && !matching_source_changed(&file.path, &changed_paths)
        {
            risky.push(PrPreviewRiskyFile {
                path: file.path.clone(),
                reason: "generated artifact changed without the corresponding source".to_string(),
            });
        } else if touches_workflow_or_tooling(&file.path) {
            risky.push(PrPreviewRiskyFile {
                path: file.path.clone(),
                reason: "workflow/tooling surface changed; reviewers will expect tight scope and strong notes".to_string(),
            });
        }
    }
    risky
}

fn enrich_issue(
    repo_root: &Path,
    work_tree_root: Option<&Path>,
    base: &ResolvedBase,
    template: IssueTemplate,
) -> Result<PrPreviewIssue> {
    let diff_hunk = if let Some(path) = template.path.as_deref() {
        first_diff_hunk(repo_root, work_tree_root, &base.merge_base, path)
            .ok()
            .flatten()
    } else {
        None
    };
    let external_ref = issue_external_ref(
        template.severity,
        template.rule,
        template.path.as_deref(),
        &template.title,
    );
    Ok(PrPreviewIssue {
        external_ref,
        severity: template.severity.to_string(),
        rule: template.rule.to_string(),
        title: template.title,
        summary: template.summary,
        path: template.path,
        line: diff_hunk
            .as_ref()
            .and_then(|text| parse_added_start_line(text))
            .map(|value| value as u64),
        diff_hunk,
        evidence: template.evidence,
        fix_hint: template.fix_hint,
        validation_hint: template.validation_hint,
        prevention_hint: template.prevention_hint,
    })
}

fn render_issue_body(issue: &PrPreviewIssue) -> String {
    let mut lines = vec![
        format!("{}: {}", issue.severity.to_uppercase(), issue.title),
        String::new(),
        format!("Rule: `{}`", issue.rule),
        issue.summary.clone(),
    ];
    if !issue.evidence.is_empty() {
        lines.push(String::new());
        lines.push("Evidence:".to_string());
        for evidence in &issue.evidence {
            lines.push(format!("- {}", evidence));
        }
    }
    lines.push(String::new());
    lines.push(format!("Suggested fix: {}", issue.fix_hint));
    lines.push(format!("Validation: {}", issue.validation_hint));
    lines.push(format!("Prevention: {}", issue.prevention_hint));
    lines.join("\n")
}

fn build_pr_copy(
    head_ref: &str,
    compare_label: &str,
    changed_files: &[PrPreviewChangedFile],
    validation_results: &[PrPreviewValidationSignal],
) -> (String, String) {
    let title = build_preview_title(head_ref, changed_files);
    let mut summary_lines = Vec::new();
    if let Some(primary) = changed_files.first() {
        summary_lines.push(format!(
            "- Tighten {} changes in `{}`.",
            primary.category, primary.path
        ));
    }
    if changed_files.len() > 1 {
        summary_lines.push(format!(
            "- Scope includes {} changed files across the preview diff.",
            changed_files.len()
        ));
    }
    if summary_lines.is_empty() {
        summary_lines
            .push("- Tighten the current review branch before drafting the PR.".to_string());
    }

    let validation_lines: Vec<String> = validation_results
        .iter()
        .map(|signal| format!("- {}: {}", signal.name, signal.detail))
        .collect();

    let body = format!(
        "## Summary\n{}\n\n## Validation\n{}\n\n## Preview\n- Compare: `{}`",
        summary_lines.join("\n"),
        validation_lines.join("\n"),
        compare_label
    );
    (title, body)
}

fn build_preview_title(head_ref: &str, changed_files: &[PrPreviewChangedFile]) -> String {
    if let Some(branch_title) = branch_title_hint(head_ref) {
        return branch_title;
    }
    if let Some(primary) = changed_files.first() {
        let scope = primary.category.replace('-', " ");
        return format!("Tighten {}", title_case(&scope));
    }
    "Tighten current change".to_string()
}

fn branch_title_hint(head_ref: &str) -> Option<String> {
    let raw = head_ref
        .split('/')
        .last()
        .unwrap_or(head_ref)
        .replace(['_', '-'], " ");
    let cleaned = raw
        .split_whitespace()
        .filter(|part| !matches!(*part, "review" | "codex" | "home" | "wip"))
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(title_case(trimmed))
    }
}

fn title_case(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            format!("{}{}", first.to_uppercase(), chars.as_str())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn collect_changed_files(
    repo_root: &Path,
    work_tree_root: Option<&Path>,
    merge_base: &str,
) -> Result<Vec<PrPreviewChangedFile>> {
    let name_status = git_capture_for_preview(
        repo_root,
        work_tree_root,
        &["diff", "--name-status", "--find-renames", merge_base, "--"],
    )?;
    let numstat = git_capture_for_preview(
        repo_root,
        work_tree_root,
        &["diff", "--numstat", "--find-renames", merge_base, "--"],
    )?;

    let mut stats_by_path: HashMap<String, (u64, u64)> = HashMap::new();
    for line in numstat
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let mut parts = line.split('\t');
        let additions = parts.next().unwrap_or("0");
        let deletions = parts.next().unwrap_or("0");
        let first_path = parts.next().unwrap_or_default();
        let second_path = parts.next();
        let path = second_path.unwrap_or(first_path).trim();
        if path.is_empty() {
            continue;
        }
        stats_by_path.insert(
            path.to_string(),
            (
                additions.parse::<u64>().unwrap_or(0),
                deletions.parse::<u64>().unwrap_or(0),
            ),
        );
    }

    let mut files = Vec::new();
    for line in name_status
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let status = parts[0].trim().to_string();
        let path = parts.last().copied().unwrap_or_default().trim().to_string();
        if path.is_empty() {
            continue;
        }
        let (additions, deletions) = stats_by_path.get(&path).copied().unwrap_or((0, 0));
        files.push(PrPreviewChangedFile {
            category: categorize_path(&path),
            risky: is_risky_path(&path),
            ui: is_ui_path(&path),
            test: is_test_path(&path),
            docs: is_docs_path(&path),
            path,
            status,
            additions,
            deletions,
        });
    }
    Ok(files)
}

fn resolve_compare_base(
    repo_root: &Path,
    head_ref: &str,
    compare_head_label: &str,
    requested_base: &str,
) -> Result<ResolvedBase> {
    let requested = requested_base.trim();
    let mut candidates = Vec::new();
    if !requested.is_empty() {
        if requested.contains('/') {
            candidates.push(requested.to_string());
        } else {
            candidates.push(format!("origin/{requested}"));
            candidates.push(requested.to_string());
        }
    }
    if let Some(origin_head) = detect_origin_head(repo_root)? {
        if !candidates.contains(&origin_head) {
            candidates.push(origin_head);
        }
    }
    candidates.push("HEAD^".to_string());

    for (index, candidate) in candidates.iter().enumerate() {
        if git_rev_parse(repo_root, candidate).is_err() {
            continue;
        }
        if let Ok(merge_base) = git_capture_in(repo_root, &["merge-base", head_ref, candidate]) {
            let compare_label = format!("{compare_head_label} vs {}", candidate);
            return Ok(ResolvedBase {
                requested: requested.to_string(),
                resolved: candidate.clone(),
                merge_base,
                compare_label,
                fallback_used: index >= 2,
            });
        }
    }

    let head_sha = git_rev_parse(repo_root, head_ref)?;
    Ok(ResolvedBase {
        requested: requested.to_string(),
        resolved: head_ref.to_string(),
        merge_base: head_sha,
        compare_label: compare_head_label.to_string(),
        fallback_used: true,
    })
}

fn resolve_git_repo_root(start: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .current_dir(start)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| format!("failed to run git rev-parse in {}", start.display()))?;
    if !output.status.success() {
        bail!("{} is not inside a git repository", start.display());
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

fn resolve_preview_context(start: &Path) -> Result<PreviewContext> {
    if let Ok(repo_root) = resolve_git_repo_root(start) {
        let head_label = detect_head_label(&repo_root)?;
        return Ok(PreviewContext {
            repo_root: repo_root.clone(),
            review_root: repo_root,
            head_ref: "HEAD".to_string(),
            head_label,
            compare_head_label: "HEAD".to_string(),
            work_tree_root: None,
        });
    }

    let review_root = find_jj_workspace_root(start).ok_or_else(|| {
        anyhow::anyhow!(
            "{} is not inside a git repository or jj workspace",
            start.display()
        )
    })?;
    let repo_root = resolve_repo_root_from_jj_workspace(&review_root)?;
    let head_label = detect_jj_head_ref(&review_root)?;
    Ok(PreviewContext {
        repo_root,
        review_root: review_root.clone(),
        head_ref: head_label.clone(),
        head_label: head_label.clone(),
        compare_head_label: head_label,
        work_tree_root: Some(review_root),
    })
}

fn find_jj_workspace_root(start: &Path) -> Option<PathBuf> {
    let current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    current
        .ancestors()
        .find(|candidate| candidate.join(".jj").join("repo").is_file())
        .map(Path::to_path_buf)
}

fn resolve_repo_root_from_jj_workspace(workspace_root: &Path) -> Result<PathBuf> {
    let pointer_path = workspace_root.join(".jj").join("repo");
    let pointer = fs::read_to_string(&pointer_path)
        .with_context(|| format!("failed to read {}", pointer_path.display()))?;
    let pointer = pointer.trim();
    if pointer.is_empty() {
        bail!("{} has an empty .jj/repo pointer", workspace_root.display());
    }
    let jj_repo_dir = if Path::new(pointer).is_absolute() {
        PathBuf::from(pointer)
    } else {
        workspace_root.join(".jj").join(pointer)
    }
    .canonicalize()
    .with_context(|| {
        format!(
            "failed to resolve JJ repo pointer for {}",
            workspace_root.display()
        )
    })?;
    let repo_root = jj_repo_dir
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "failed to derive shared repo root from {}",
                jj_repo_dir.display()
            )
        })?;
    if !repo_root.join(".git").exists() {
        bail!(
            "derived shared repo root {} does not contain .git",
            repo_root.display()
        );
    }
    Ok(repo_root)
}

fn detect_jj_head_ref(workspace_root: &Path) -> Result<String> {
    let output = Command::new("jj")
        .current_dir(workspace_root)
        .args(["log", "-r", "@ | @-", "--no-graph", "-T", "bookmarks"])
        .output()
        .with_context(|| format!("failed to run jj log in {}", workspace_root.display()))?;
    if !output.status.success() {
        bail!("jj log failed in {}", workspace_root.display());
    }
    let mut bookmarks = parse_jj_bookmark_tokens(&String::from_utf8_lossy(&output.stdout));
    bookmarks.sort();
    bookmarks.dedup();
    if let Some(leaf) = bookmarks
        .iter()
        .find(|token| token.starts_with("review/") || token.starts_with("codex/"))
    {
        return Ok(leaf.clone());
    }
    if let Some(stable) = bookmarks
        .iter()
        .find(|token| !token.starts_with("recovery/") && !token.starts_with("backup/"))
    {
        return Ok(stable.clone());
    }
    bookmarks.into_iter().next().ok_or_else(|| {
        anyhow::anyhow!(
            "could not infer current jj bookmark in {}",
            workspace_root.display()
        )
    })
}

fn parse_jj_bookmark_tokens(output: &str) -> Vec<String> {
    output
        .split_whitespace()
        .flat_map(|token| token.split(['*', '?']))
        .map(str::trim)
        .filter(|token| !token.is_empty() && !token.contains('@'))
        .map(ToOwned::to_owned)
        .collect()
}

fn detect_repo_label(repo_root: &Path) -> Result<String> {
    let remote_url =
        git_capture_in(repo_root, &["config", "--get", "remote.origin.url"]).unwrap_or_default();
    let raw = remote_url.trim();
    if raw.is_empty() {
        return Ok(repo_root
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("repo")
            .to_string());
    }
    if let Some(index) = raw.rfind(':') {
        let tail = raw[index + 1..].trim_end_matches(".git");
        if tail.contains('/') {
            return Ok(tail.to_string());
        }
    }
    if let Some((_, tail)) = raw.rsplit_once("github.com/") {
        return Ok(tail
            .trim_end_matches(".git")
            .trim_start_matches('/')
            .to_string());
    }
    Ok(raw.to_string())
}

fn detect_head_label(repo_root: &Path) -> Result<String> {
    let branch = git_capture_in(repo_root, &["branch", "--show-current"]).unwrap_or_default();
    if !branch.trim().is_empty() {
        return Ok(branch.trim().to_string());
    }
    let short_sha = git_capture_in(repo_root, &["rev-parse", "--short", "HEAD"])?;
    Ok(format!("detached-{short_sha}"))
}

fn detect_origin_head(repo_root: &Path) -> Result<Option<String>> {
    match git_capture_in(
        repo_root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value.trim().to_string())),
        _ => Ok(None),
    }
}

fn git_rev_parse(repo_root: &Path, rev: &str) -> Result<String> {
    git_capture_in(repo_root, &["rev-parse", "--verify", "--quiet", rev])
}

fn git_capture_in(repo_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| {
            format!(
                "failed to run git {} in {}",
                args.join(" "),
                repo_root.display()
            )
        })?;
    if !output.status.success() {
        bail!("git {} failed in {}", args.join(" "), repo_root.display());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_capture_for_preview(
    repo_root: &Path,
    work_tree_root: Option<&Path>,
    args: &[&str],
) -> Result<String> {
    let mut command = Command::new("git");
    if let Some(work_tree_root) = work_tree_root {
        command
            .arg("--git-dir")
            .arg(repo_root.join(".git"))
            .arg("--work-tree")
            .arg(work_tree_root)
            .current_dir(work_tree_root);
    } else {
        command.current_dir(repo_root);
    }
    let output = command.args(args).output().with_context(|| {
        format!(
            "failed to run git {} for preview in {}",
            args.join(" "),
            work_tree_root.unwrap_or(repo_root).display()
        )
    })?;
    if !output.status.success() {
        bail!(
            "git {} failed for preview in {}",
            args.join(" "),
            work_tree_root.unwrap_or(repo_root).display()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn first_diff_hunk(
    repo_root: &Path,
    work_tree_root: Option<&Path>,
    merge_base: &str,
    relative_path: &str,
) -> Result<Option<String>> {
    let patch = git_capture_for_preview(
        repo_root,
        work_tree_root,
        &[
            "diff",
            "--unified=3",
            "--find-renames",
            merge_base,
            "--",
            relative_path,
        ],
    )?;
    if patch.trim().is_empty() {
        return Ok(None);
    }
    let normalized = patch.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.lines().collect();
    let Some(first_hunk_index) = lines.iter().position(|line| line.starts_with("@@ ")) else {
        return Ok(Some(normalized.trim().to_string()));
    };
    let prelude = &lines[..first_hunk_index];
    let mut next_index = first_hunk_index + 1;
    while next_index < lines.len() && !lines[next_index].starts_with("@@ ") {
        next_index += 1;
    }
    let mut selected = Vec::new();
    selected.extend_from_slice(prelude);
    selected.extend_from_slice(&lines[first_hunk_index..next_index]);
    Ok(Some(selected.join("\n").trim().to_string()))
}

fn parse_added_start_line(diff_hunk: &str) -> Option<usize> {
    diff_hunk
        .lines()
        .find(|line| line.starts_with("@@ "))
        .and_then(|header| {
            let after_plus = header.split(" +").nth(1)?;
            let number = after_plus
                .split([' ', ','])
                .next()?
                .trim()
                .parse::<usize>()
                .ok()?;
            Some(number)
        })
}

fn issue_external_ref(severity: &str, rule: &str, path: Option<&str>, title: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(severity.as_bytes());
    hasher.update(b"\n");
    hasher.update(rule.as_bytes());
    hasher.update(b"\n");
    hasher.update(path.unwrap_or_default().as_bytes());
    hasher.update(b"\n");
    hasher.update(title.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("preview:{severity}:{rule}:{}", &digest[..12])
}

fn empty_check_item(external_ref: &str) -> PrPreviewCheckItem {
    PrPreviewCheckItem {
        external_ref: external_ref.to_string(),
        concern_status: String::new(),
        verdict: String::new(),
        why: String::new(),
        fix: String::new(),
        validation: String::new(),
        prevention: String::new(),
        coach_findings: String::new(),
        kit_upgrade: String::new(),
        ledger_update: String::new(),
        patched: false,
        validated: false,
        prevention_captured: false,
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    fs::write(path, format!("{}\n", serde_json::to_string_pretty(value)?))
        .with_context(|| format!("failed to write {}", path.display()))
}

fn categorize_path(path: &str) -> String {
    let normalized = path.trim_start_matches("./");
    if normalized.is_empty() {
        return "root".to_string();
    }
    let first = normalized.split('/').next().unwrap_or(normalized);
    if matches!(first, "src" | "app" | "lib" | "ide" | "packages") {
        return first.to_string();
    }
    if is_docs_path(path) {
        return "docs".to_string();
    }
    if is_test_path(path) {
        return "tests".to_string();
    }
    first.to_string()
}

fn is_risky_path(path: &str) -> bool {
    is_internal_overlay_path(path)
        || is_lockfile_path(path)
        || is_generated_artifact_path(path)
        || touches_workflow_or_tooling(path)
}

fn is_docs_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md")
        || lower.starts_with("docs/")
        || lower.contains("/docs/")
        || lower.ends_with(".adoc")
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("tests/")
        || lower.contains("/tests/")
        || lower.contains(".test.")
        || lower.contains(".spec.")
        || lower.ends_with("_test.rs")
}

fn is_ui_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".tsx")
        || lower.ends_with(".jsx")
        || lower.ends_with(".css")
        || lower.ends_with(".scss")
        || lower.contains("/components/")
        || lower.contains("/ui/")
        || lower.contains("/viewer/")
}

fn is_manifest_path(path: &str) -> bool {
    matches!(
        path,
        "package.json"
            | "pnpm-lock.yaml"
            | "package-lock.json"
            | "yarn.lock"
            | "Cargo.toml"
            | "Cargo.lock"
            | "pyproject.toml"
            | "poetry.lock"
    ) || path.ends_with("/package.json")
        || path.ends_with("/Cargo.toml")
        || path.ends_with("/pyproject.toml")
}

fn is_lockfile_path(path: &str) -> bool {
    matches!(
        path,
        "pnpm-lock.yaml" | "package-lock.json" | "yarn.lock" | "Cargo.lock" | "poetry.lock"
    ) || path.ends_with("/pnpm-lock.yaml")
        || path.ends_with("/package-lock.json")
        || path.ends_with("/yarn.lock")
        || path.ends_with("/Cargo.lock")
        || path.ends_with("/poetry.lock")
}

fn is_generated_artifact_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("dist/")
        || lower.starts_with("build/")
        || lower.starts_with("coverage/")
        || lower.starts_with("vendor/")
        || lower.contains("/dist/")
        || lower.contains("/build/")
        || lower.contains("/coverage/")
        || lower.ends_with(".min.js")
        || lower.ends_with(".generated.ts")
        || lower.ends_with(".generated.js")
}

fn is_visual_artifact_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
        || lower.ends_with(".mov")
        || lower.ends_with(".mp4")
}

fn is_internal_overlay_path(path: &str) -> bool {
    let normalized = path.trim_start_matches("./");
    normalized.starts_with(".ai/")
        || normalized.starts_with(".claude/")
        || normalized.starts_with(".jj/")
        || normalized.starts_with(".codex/")
}

fn touches_workflow_or_tooling(path: &str) -> bool {
    let normalized = path.trim_start_matches("./");
    normalized.starts_with(".github/")
        || normalized.starts_with("scripts/")
        || normalized.starts_with("tools/")
        || normalized.starts_with("docs/")
        || normalized.contains("flow-vscode-extension")
        || normalized.ends_with("flow.toml")
        || normalized.ends_with("package.json")
        || normalized.ends_with("Cargo.toml")
        || normalized.ends_with("tsconfig.json")
        || normalized.ends_with(".vscode/settings.json")
}

fn matching_manifest_changed(path: &str, changed_paths: &HashSet<&str>) -> bool {
    if path.ends_with("Cargo.lock") {
        return changed_paths.contains("Cargo.toml")
            || changed_paths
                .iter()
                .any(|item| item.ends_with("/Cargo.toml"));
    }
    if path.ends_with("pnpm-lock.yaml")
        || path.ends_with("package-lock.json")
        || path.ends_with("yarn.lock")
    {
        return changed_paths.contains("package.json")
            || changed_paths
                .iter()
                .any(|item| item.ends_with("/package.json"));
    }
    if path.ends_with("poetry.lock") {
        return changed_paths.contains("pyproject.toml")
            || changed_paths
                .iter()
                .any(|item| item.ends_with("/pyproject.toml"));
    }
    true
}

fn matching_source_changed(path: &str, changed_paths: &HashSet<&str>) -> bool {
    if path.ends_with(".generated.ts") {
        let source = path.trim_end_matches(".generated.ts").to_string() + ".ts";
        return changed_paths.contains(source.as_str());
    }
    if path.starts_with("dist/") {
        return changed_paths
            .iter()
            .any(|candidate| candidate.starts_with("src/"));
    }
    if path.starts_with("build/") {
        return changed_paths.iter().any(|candidate| {
            candidate.starts_with("src/")
                || candidate.ends_with("package.json")
                || candidate.ends_with("Cargo.toml")
        });
    }
    false
}

fn pr_copy_is_risky(title: &str, body: &str) -> bool {
    let lower_title = title.to_ascii_lowercase();
    let lower_body = body.to_ascii_lowercase();
    ["wip", "misc", "stuff", "things", "quick", "just", "cleanup"]
        .iter()
        .any(|needle| lower_title.contains(needle) || lower_body.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .expect("git status");
        assert!(status.success(), "git {:?} failed", args);
    }

    fn git_output(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("git output");
        assert!(output.status.success(), "git {:?} failed", args);
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo() -> tempfile::TempDir {
        let temp = tempdir().expect("tempdir");
        git(temp.path(), &["init"]);
        git(temp.path(), &["config", "user.name", "Flow Test"]);
        git(temp.path(), &["config", "user.email", "flow@example.com"]);
        git(temp.path(), &["checkout", "-b", "review/demo-preview"]);
        fs::create_dir_all(temp.path().join("src")).expect("mkdir src");
        fs::write(temp.path().join("src/app.ts"), "export const value = 1;\n").expect("write app");
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-m", "initial"]);
        git(
            temp.path(),
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        git(
            temp.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        temp
    }

    #[test]
    fn parse_pr_preview_args_uses_pr_opts_fields() {
        let opts = PrOpts {
            args: vec!["preview".to_string()],
            base: "main".to_string(),
            draft: false,
            no_open: false,
            no_commit: false,
            hash: None,
            paths: vec!["/tmp/repo".to_string()],
            json: true,
            mode: Some(PrPreviewModeArg::Feedback),
        };
        let parsed = parse_pr_preview_args(&opts.args, &opts)
            .expect("parse")
            .expect("preview");
        assert_eq!(parsed.repo_path, Some(PathBuf::from("/tmp/repo")));
        assert_eq!(parsed.requested_base, "main");
        assert!(parsed.json);
        assert_eq!(parsed.mode, PrPreviewModeArg::Feedback);
    }

    #[test]
    fn build_preview_packet_flags_missing_validation() {
        let repo = init_repo();
        fs::write(repo.path().join("src/app.ts"), "export const value = 2;\n").expect("update app");

        let context = resolve_preview_context(repo.path()).expect("context");
        let packet = build_preview_packet(
            &context,
            &PrPreviewCommand {
                repo_path: None,
                requested_base: "main".to_string(),
                mode: PrPreviewModeArg::Draft,
                json: true,
            },
        )
        .expect("packet");

        assert_eq!(packet.preview_status, "warning");
        assert_eq!(packet.blockers_count, 0);
        assert!(packet.warnings_count >= 1);
        assert!(
            packet
                .warnings
                .iter()
                .any(|issue| issue.rule == "missing-validation")
        );
        assert_eq!(packet.compare_label, "HEAD vs origin/main");
    }

    #[test]
    fn write_preview_artifacts_creates_canonical_and_bundle_files() {
        let repo = init_repo();
        fs::write(repo.path().join("src/app.ts"), "export const value = 2;\n").expect("update app");

        let context = resolve_preview_context(repo.path()).expect("context");
        let packet = build_preview_packet(
            &context,
            &PrPreviewCommand {
                repo_path: None,
                requested_base: "main".to_string(),
                mode: PrPreviewModeArg::Draft,
                json: true,
            },
        )
        .expect("packet");
        let result = write_preview_artifacts(repo.path(), &packet).expect("write artifacts");
        assert_eq!(result.status, "created");
        assert!(
            repo.path()
                .join(".ai/reviews/flow-pr-preview.json")
                .exists()
        );
        assert!(repo.path().join(".ai/reviews/flow-pr-preview.md").exists());
        assert!(repo.path().join(".ai/reviews/flow-pr-create.json").exists());
        assert!(
            repo.path()
                .join(".ai/reviews/flow-review-preview-state.json")
                .exists()
        );
        assert!(
            repo.path()
                .join(".ai/reviews/flow-review-preview-check.json")
                .exists()
        );

        let state: PrPreviewReviewState = read_json(
            &repo
                .path()
                .join(".ai/reviews/flow-review-preview-state.json"),
        )
        .expect("state");
        assert!(state.review_plan.ends_with("flow-pr-preview.md"));
        assert!(state.snapshot_json.ends_with("flow-pr-preview.json"));
    }

    #[test]
    fn resolve_compare_base_uses_origin_main_when_available() {
        let repo = init_repo();
        let base = resolve_compare_base(repo.path(), "HEAD", "HEAD", "main").expect("base");
        assert_eq!(base.resolved, "origin/main");
        assert_eq!(base.compare_label, "HEAD vs origin/main");
    }

    #[test]
    fn parse_jj_bookmark_tokens_splits_current_marker_noise() {
        let tokens =
            parse_jj_bookmark_tokens("review/home-cad-viewer-main*main recovery/default-home");
        assert_eq!(
            tokens,
            vec![
                "review/home-cad-viewer-main",
                "main",
                "recovery/default-home",
            ]
        );
    }

    #[test]
    fn jj_workspace_helpers_resolve_shared_repo_root() {
        let repo = init_repo();
        fs::create_dir_all(repo.path().join(".jj/repo")).expect("mkdir repo jj");

        let workspace = tempdir().expect("workspace tempdir");
        fs::create_dir_all(workspace.path().join(".jj")).expect("mkdir workspace jj");
        fs::create_dir_all(workspace.path().join("apps/editor")).expect("mkdir nested");
        fs::write(
            workspace.path().join(".jj/repo"),
            repo.path().join(".jj/repo").display().to_string(),
        )
        .expect("write jj repo pointer");

        let found_workspace =
            find_jj_workspace_root(&workspace.path().join("apps/editor")).expect("workspace root");
        assert_eq!(found_workspace, workspace.path());
        let repo_root = resolve_repo_root_from_jj_workspace(workspace.path()).expect("repo root");
        assert_eq!(
            repo_root,
            repo.path().canonicalize().expect("canonical repo root")
        );
    }

    #[test]
    fn build_preview_title_prefers_branch_hint() {
        let title = build_preview_title("review/cad-viewer-preview", &[]);
        assert_eq!(title, "Cad Viewer Preview");
    }

    #[test]
    fn clear_preview_artifacts_removes_generated_files() {
        let repo = init_repo();
        let reviews_dir = repo.path().join(".ai/reviews");
        fs::create_dir_all(&reviews_dir).expect("mkdir reviews");
        for name in [
            FLOW_PR_PREVIEW_JSON_FILENAME,
            FLOW_PR_PREVIEW_MD_FILENAME,
            FLOW_PR_CREATE_FILENAME,
            FLOW_REVIEW_PREVIEW_STATE_FILENAME,
            FLOW_REVIEW_PREVIEW_CHECK_FILENAME,
        ] {
            fs::write(reviews_dir.join(name), "{}\n").expect("write artifact");
        }
        clear_preview_artifacts(repo.path()).expect("clear");
        assert!(!reviews_dir.join(FLOW_PR_PREVIEW_JSON_FILENAME).exists());
        assert!(
            !reviews_dir
                .join(FLOW_REVIEW_PREVIEW_STATE_FILENAME)
                .exists()
        );
        let head = git_output(repo.path(), &["rev-parse", "--short", "HEAD"]);
        assert!(!head.is_empty());
    }
}
