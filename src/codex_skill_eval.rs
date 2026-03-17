use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{codex_memory, codex_text, config};

const SKILL_EVAL_VERSION: u32 = 1;
const SKILL_EVAL_REVERSE_SCAN_CHUNK_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexSkillEvalEvent {
    pub version: u32,
    pub recorded_at_unix: u64,
    pub mode: String,
    pub action: String,
    pub route: String,
    pub target_path: String,
    pub launch_path: String,
    pub query: String,
    #[serde(default)]
    pub session_id: Option<String>,
    pub runtime_token: Option<String>,
    pub runtime_skills: Vec<String>,
    pub prompt_context_budget_chars: usize,
    pub prompt_chars: usize,
    pub injected_context_chars: usize,
    pub reference_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexSkillScore {
    pub name: String,
    pub sample_size: usize,
    pub outcome_samples: usize,
    pub pass_rate: f64,
    pub avg_affinity: f64,
    pub baseline_affinity: f64,
    pub normalized_gain: f64,
    pub avg_context_chars: f64,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexSkillOutcomeEvent {
    pub version: u32,
    pub recorded_at_unix: u64,
    pub runtime_token: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub target_path: Option<String>,
    pub kind: String,
    pub skill_names: Vec<String>,
    pub artifact_path: Option<String>,
    pub success: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CodexSkillScorecard {
    pub version: u32,
    pub generated_at_unix: u64,
    pub target_path: String,
    pub samples: usize,
    pub skills: Vec<CodexSkillScore>,
}

#[derive(Default)]
struct SkillAggregate {
    count: usize,
    outcome_count: usize,
    success_sum: f64,
    total_affinity_used: f64,
    total_affinity_all: f64,
    total_context_chars: usize,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

fn skill_eval_root() -> Result<PathBuf> {
    Ok(config::ensure_global_state_dir()?
        .join("codex")
        .join("skill-eval"))
}

fn skill_eval_roots() -> Vec<PathBuf> {
    config::global_state_dir_candidates()
        .into_iter()
        .map(|root| root.join("codex").join("skill-eval"))
        .collect()
}

fn load_events_from_paths(
    paths: Vec<PathBuf>,
    target_path: Option<&Path>,
    limit: usize,
) -> Result<Vec<CodexSkillEvalEvent>> {
    let mut events = Vec::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        let path_ref = path.as_path();
        let _ = visit_lines_reverse(path_ref, limit, |line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None::<CodexSkillEvalEvent>;
            }
            let mut event = serde_json::from_str::<CodexSkillEvalEvent>(trimmed).ok()?;
            event.query = codex_text::sanitize_codex_query_text(&event.query)?;
            if let Some(filter) = target_path
                && !path_matches(&event, filter)
            {
                return None;
            }
            Some(event)
        })?
        .map(|mut loaded| events.append(&mut loaded));
    }

    events.sort_by(|a, b| b.recorded_at_unix.cmp(&a.recorded_at_unix));
    if events.len() > limit {
        events.truncate(limit);
    }
    Ok(events)
}

fn load_outcomes_from_paths(
    paths: Vec<PathBuf>,
    target_path: Option<&Path>,
    limit: usize,
) -> Result<Vec<CodexSkillOutcomeEvent>> {
    let mut outcomes = Vec::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        let path_ref = path.as_path();
        let _ = visit_lines_reverse(path_ref, limit, |line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None::<CodexSkillOutcomeEvent>;
            }
            let outcome = serde_json::from_str::<CodexSkillOutcomeEvent>(trimmed).ok()?;
            if let Some(filter) = target_path {
                let Some(target) = outcome.target_path.as_deref() else {
                    return None;
                };
                let filter = filter.display().to_string();
                if target != filter && !target.starts_with(&(filter + "/")) {
                    return None;
                }
            }
            Some(outcome)
        })?
        .map(|mut loaded| outcomes.append(&mut loaded));
    }

    outcomes.sort_by(|a, b| b.recorded_at_unix.cmp(&a.recorded_at_unix));
    if outcomes.len() > limit {
        outcomes.truncate(limit);
    }
    Ok(outcomes)
}

fn events_path() -> Result<PathBuf> {
    let root = skill_eval_root()?;
    fs::create_dir_all(&root)?;
    Ok(root.join("events.jsonl"))
}

fn outcomes_path() -> Result<PathBuf> {
    let root = skill_eval_root()?;
    fs::create_dir_all(&root)?;
    Ok(root.join("outcomes.jsonl"))
}

fn scorecards_dir() -> Result<PathBuf> {
    let dir = skill_eval_root()?.join("scorecards");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn scorecard_key(target_path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(target_path.display().to_string().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    digest[..12.min(digest.len())].to_string()
}

fn scorecard_path(target_path: &Path) -> Result<PathBuf> {
    Ok(scorecards_dir()?.join(format!("{}.json", scorecard_key(target_path))))
}

fn tokenize_words(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase())
        .filter(|part| {
            part.len() >= 4
                && !matches!(
                    part.as_str(),
                    "flow"
                        | "runtime"
                        | "skill"
                        | "skills"
                        | "this"
                        | "that"
                        | "with"
                        | "from"
                        | "into"
                        | "write"
                        | "using"
                        | "codex"
                        | "session"
                        | "query"
                        | "prompt"
                        | "plan"
                )
        })
        .collect()
}

fn affinity_for_skill(skill_name: &str, query: &str) -> f64 {
    let query_lower = query.to_ascii_lowercase();
    let skill_words = tokenize_words(skill_name);
    if skill_words.is_empty() {
        return 0.0;
    }

    let phrase = skill_words.join(" ");
    if !phrase.is_empty() && query_lower.contains(&phrase) {
        return 1.0;
    }

    let hits = skill_words
        .iter()
        .filter(|word| query_lower.contains(word.as_str()))
        .count();
    hits as f64 / skill_words.len() as f64
}

fn calculate_normalized_gain(p_with: f64, p_without: f64) -> f64 {
    if p_without >= 1.0 {
        return if p_with >= 1.0 { 0.0 } else { -1.0 };
    }
    (p_with - p_without) / (1.0 - p_without)
}

fn path_matches(event: &CodexSkillEvalEvent, target_path: &Path) -> bool {
    let target = target_path.display().to_string();
    event.target_path == target
        || event.launch_path == target
        || event.target_path.starts_with(&(target.clone() + "/"))
        || event.launch_path.starts_with(&(target + "/"))
}

pub fn log_event(event: &CodexSkillEvalEvent) -> Result<()> {
    let mut sanitized = event.clone();
    let Some(query) = codex_text::sanitize_codex_query_text(&sanitized.query) else {
        return Ok(());
    };
    sanitized.query = query;
    let path = events_path()?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::to_writer(&mut file, &sanitized)
        .context("failed to encode codex skill-eval event")?;
    file.write_all(b"\n")
        .context("failed to terminate codex skill-eval event")?;
    let _ = codex_memory::mirror_skill_eval_event(&sanitized);
    Ok(())
}

pub fn log_outcome(outcome: &CodexSkillOutcomeEvent) -> Result<()> {
    let path = outcomes_path()?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::to_writer(&mut file, outcome)
        .context("failed to encode codex skill-eval outcome")?;
    file.write_all(b"\n")
        .context("failed to terminate codex skill-eval outcome")?;
    let _ = codex_memory::mirror_skill_outcome_event(outcome);
    Ok(())
}

pub fn event_count() -> usize {
    load_events(None, usize::MAX)
        .map(|events| events.len())
        .unwrap_or(0)
}

pub fn outcome_count() -> usize {
    load_outcomes(None, usize::MAX)
        .map(|outcomes| outcomes.len())
        .unwrap_or(0)
}

pub fn load_events(target_path: Option<&Path>, limit: usize) -> Result<Vec<CodexSkillEvalEvent>> {
    load_events_from_paths(
        skill_eval_roots()
            .into_iter()
            .map(|root| root.join("events.jsonl"))
            .collect(),
        target_path,
        limit,
    )
}

fn collect_recent_targets(
    events: Vec<CodexSkillEvalEvent>,
    max_targets: usize,
    within_hours: u64,
) -> Vec<PathBuf> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let cutoff = unix_now().saturating_sub(within_hours.saturating_mul(3600));
    for event in events {
        if event.recorded_at_unix < cutoff {
            continue;
        }
        if event.target_path.trim().is_empty() {
            continue;
        }
        let path = PathBuf::from(&event.target_path);
        if !path.exists() {
            continue;
        }
        if seen.insert(event.target_path.clone()) {
            out.push(path);
            if out.len() >= max_targets {
                break;
            }
        }
    }
    out
}

pub fn recent_targets(limit: usize, max_targets: usize, within_hours: u64) -> Result<Vec<PathBuf>> {
    Ok(collect_recent_targets(
        load_events(None, limit)?,
        max_targets,
        within_hours,
    ))
}

pub fn load_outcomes(
    target_path: Option<&Path>,
    limit: usize,
) -> Result<Vec<CodexSkillOutcomeEvent>> {
    load_outcomes_from_paths(
        skill_eval_roots()
            .into_iter()
            .map(|root| root.join("outcomes.jsonl"))
            .collect(),
        target_path,
        limit,
    )
}

pub fn rebuild_scorecard(target_path: &Path, limit: usize) -> Result<CodexSkillScorecard> {
    let events = load_events(Some(target_path), limit)?;
    let outcomes = load_outcomes(Some(target_path), limit)?;
    let scorecard = build_scorecard(target_path, events, outcomes);
    let path = scorecard_path(target_path)?;
    fs::write(&path, serde_json::to_vec_pretty(&scorecard)?)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(scorecard)
}

fn build_scorecard(
    target_path: &Path,
    events: Vec<CodexSkillEvalEvent>,
    outcomes: Vec<CodexSkillOutcomeEvent>,
) -> CodexSkillScorecard {
    let outcomes_by_token = outcomes
        .iter()
        .filter_map(|outcome| {
            outcome
                .runtime_token
                .as_deref()
                .map(|token| (token.to_string(), outcome))
        })
        .fold(
            HashMap::<String, Vec<&CodexSkillOutcomeEvent>>::new(),
            |mut acc, (token, outcome)| {
                acc.entry(token).or_default().push(outcome);
                acc
            },
        );
    let outcomes_by_session = outcomes
        .iter()
        .filter_map(|outcome| {
            outcome
                .session_id
                .as_deref()
                .map(|session_id| (session_id.to_string(), outcome))
        })
        .fold(
            HashMap::<String, Vec<&CodexSkillOutcomeEvent>>::new(),
            |mut acc, (session_id, outcome)| {
                acc.entry(session_id).or_default().push(outcome);
                acc
            },
        );
    let known_skills = events
        .iter()
        .flat_map(|event| event.runtime_skills.iter().cloned())
        .collect::<std::collections::BTreeSet<_>>();
    let mut aggregates = known_skills
        .iter()
        .map(|name| (name.clone(), SkillAggregate::default()))
        .collect::<HashMap<_, _>>();

    for event in &events {
        let query = event.query.trim();
        if query.is_empty() {
            continue;
        }
        let used = event
            .runtime_skills
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        for skill_name in &known_skills {
            let affinity = affinity_for_skill(skill_name, query);
            let entry = aggregates.entry(skill_name.clone()).or_default();
            entry.total_affinity_all += affinity;
            if used.contains(skill_name) {
                entry.count += 1;
                entry.total_affinity_used += affinity;
                entry.total_context_chars += event.injected_context_chars;
                let matched = event
                    .runtime_token
                    .as_deref()
                    .and_then(|token| outcomes_by_token.get(token))
                    .or_else(|| {
                        event
                            .session_id
                            .as_deref()
                            .and_then(|session_id| outcomes_by_session.get(session_id))
                    });
                if let Some(matched) = matched {
                    let best_success = matched
                        .iter()
                        .filter(|outcome| {
                            outcome.skill_names.is_empty()
                                || outcome.skill_names.iter().any(|name| name == skill_name)
                        })
                        .map(|outcome| outcome.success)
                        .fold(0.0f64, f64::max);
                    entry.outcome_count += 1;
                    entry.success_sum += best_success;
                }
            }
        }
    }

    let total_events = events.len().max(1) as f64;
    let baseline_pass_rate = {
        let mut success = 0.0f64;
        let mut samples = 0usize;
        for event in &events {
            let matched = event
                .runtime_token
                .as_deref()
                .and_then(|token| outcomes_by_token.get(token))
                .or_else(|| {
                    event
                        .session_id
                        .as_deref()
                        .and_then(|session_id| outcomes_by_session.get(session_id))
                });
            let Some(matched) = matched else {
                continue;
            };
            let best = matched
                .iter()
                .map(|outcome| outcome.success)
                .fold(0.0, f64::max);
            success += best;
            samples += 1;
        }
        if samples == 0 {
            0.0
        } else {
            success / samples as f64
        }
    };
    let mut skills = aggregates
        .into_iter()
        .filter_map(|(name, agg)| {
            if agg.count == 0 {
                return None;
            }
            let avg_affinity = agg.total_affinity_used / agg.count as f64;
            let baseline_affinity = agg.total_affinity_all / total_events;
            let pass_rate = if agg.outcome_count == 0 {
                0.0
            } else {
                agg.success_sum / agg.outcome_count as f64
            };
            let normalized_gain = if agg.outcome_count > 0 {
                calculate_normalized_gain(pass_rate, baseline_pass_rate)
            } else {
                calculate_normalized_gain(avg_affinity, baseline_affinity)
            };
            let avg_context_chars = agg.total_context_chars as f64 / agg.count as f64;
            let score = if agg.outcome_count > 0 {
                (normalized_gain * 100.0) + (pass_rate * 25.0) + (agg.count.min(20) as f64 / 4.0)
                    - (avg_context_chars / 500.0)
            } else {
                (normalized_gain * 100.0) + (agg.count.min(20) as f64 / 4.0)
                    - (avg_context_chars / 500.0)
            };
            Some(CodexSkillScore {
                name,
                sample_size: agg.count,
                outcome_samples: agg.outcome_count,
                pass_rate,
                avg_affinity,
                baseline_affinity,
                normalized_gain,
                avg_context_chars,
                score,
            })
        })
        .collect::<Vec<_>>();
    skills.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let scorecard = CodexSkillScorecard {
        version: SKILL_EVAL_VERSION,
        generated_at_unix: unix_now(),
        target_path: target_path.display().to_string(),
        samples: events.len(),
        skills,
    };
    scorecard
}

pub fn load_scorecard(target_path: &Path) -> Result<Option<CodexSkillScorecard>> {
    for root in skill_eval_roots() {
        let path = root
            .join("scorecards")
            .join(format!("{}.json", scorecard_key(target_path)));
        if !path.exists() {
            continue;
        }
        let raw = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let scorecard = serde_json::from_slice::<CodexSkillScorecard>(&raw)
            .with_context(|| format!("failed to decode {}", path.display()))?;
        return Ok(Some(scorecard));
    }
    Ok(None)
}

fn visit_lines_reverse<T, F>(
    path: &Path,
    max_items: usize,
    mut on_line: F,
) -> Result<Option<Vec<T>>>
where
    F: FnMut(&str) -> Option<T>,
{
    if max_items == 0 {
        return Ok(Some(Vec::new()));
    }

    let mut file =
        File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut pos = file.seek(SeekFrom::End(0))?;
    if pos == 0 {
        return Ok(None);
    }

    let mut chunk = vec![0u8; SKILL_EVAL_REVERSE_SCAN_CHUNK_BYTES];
    let mut carry = Vec::new();
    let mut values = Vec::new();

    while pos > 0 && values.len() < max_items {
        let read_len = usize::try_from(pos.min(chunk.len() as u64)).unwrap_or(chunk.len());
        pos -= read_len as u64;
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut chunk[..read_len])
            .with_context(|| format!("failed to read {}", path.display()))?;

        let buf = &chunk[..read_len];
        let mut end = read_len;
        while let Some(idx) = buf[..end].iter().rposition(|&byte| byte == b'\n') {
            if let Some(value) =
                process_reverse_line_segment(&buf[idx + 1..end], &mut carry, &mut on_line)
            {
                values.push(value);
                if values.len() >= max_items {
                    return Ok(Some(values));
                }
            }
            end = idx;
        }

        if end > 0 {
            let mut combined = Vec::with_capacity(end + carry.len());
            combined.extend_from_slice(&buf[..end]);
            combined.extend_from_slice(&carry);
            carry = combined;
        }
    }

    if values.len() < max_items
        && !carry.is_empty()
        && let Ok(line) = std::str::from_utf8(&carry)
        && let Some(value) = on_line(line.trim_end_matches('\r'))
    {
        values.push(value);
    }

    if values.is_empty() {
        Ok(None)
    } else {
        Ok(Some(values))
    }
}

fn process_reverse_line_segment<T, F>(
    segment: &[u8],
    carry: &mut Vec<u8>,
    on_line: &mut F,
) -> Option<T>
where
    F: FnMut(&str) -> Option<T>,
{
    if carry.is_empty() {
        let line = std::str::from_utf8(segment).ok()?;
        return on_line(line.trim_end_matches('\r'));
    }

    let suffix = std::mem::take(carry);
    let mut line_bytes = Vec::with_capacity(segment.len() + suffix.len());
    line_bytes.extend_from_slice(segment);
    line_bytes.extend_from_slice(&suffix);
    let line = std::str::from_utf8(&line_bytes).ok()?;
    on_line(line.trim_end_matches('\r'))
}

pub fn score_for_skill(target_path: &Path, name: &str) -> Option<f64> {
    load_scorecard(target_path)
        .ok()
        .flatten()
        .and_then(|scorecard| {
            scorecard
                .skills
                .into_iter()
                .find(|skill| skill.name == name)
                .map(|skill| skill.score)
        })
}

pub fn format_scorecard(scorecard: &CodexSkillScorecard) -> String {
    if scorecard.skills.is_empty() {
        return format!(
            "# codex skill-eval\n\
target: {}\n\
samples: {}\n\
skills: 0",
            scorecard.target_path, scorecard.samples
        );
    }

    let mut lines = vec![
        "# codex skill-eval".to_string(),
        format!("target: {}", scorecard.target_path),
        format!("samples: {}", scorecard.samples),
    ];
    for skill in &scorecard.skills {
        lines.push(format!(
            "- {} | score {:.2} | gain {:.3} | samples {} | outcomes {} | pass {:.2} | ctx {:.0} chars",
            skill.name,
            skill.score,
            skill.normalized_gain,
            skill.sample_size,
            skill.outcome_samples,
            skill.pass_rate,
            skill.avg_context_chars
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_gain_behaves_like_skillgrade_formula() {
        let gain = calculate_normalized_gain(0.8, 0.5);
        assert!((gain - 0.6).abs() < 0.0001);
    }

    #[test]
    fn affinity_prefers_phrase_matches() {
        assert!(
            affinity_for_skill("find-skills", "please find skills for react")
                >= affinity_for_skill("find-skills", "please help with react")
        );
    }

    #[test]
    fn load_events_reads_both_state_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy_path = dir.path().join("legacy");
        let current_path = dir.path().join("current");
        fs::create_dir_all(&legacy_path).expect("legacy dir");
        fs::create_dir_all(&current_path).expect("current dir");

        let event = CodexSkillEvalEvent {
            version: 1,
            recorded_at_unix: 1,
            mode: "resolve".to_string(),
            action: "new".to_string(),
            route: "new-plain".to_string(),
            target_path: "/tmp/repo".to_string(),
            launch_path: "/tmp/repo".to_string(),
            query: "write plan".to_string(),
            session_id: None,
            runtime_token: Some("tok".to_string()),
            runtime_skills: vec!["plan_write".to_string()],
            prompt_context_budget_chars: 400,
            prompt_chars: 100,
            injected_context_chars: 30,
            reference_count: 0,
        };
        fs::write(
            legacy_path.join("events.jsonl"),
            serde_json::to_string(&event).expect("encode") + "\n",
        )
        .expect("legacy events");
        fs::write(
            current_path.join("events.jsonl"),
            serde_json::to_string(&CodexSkillEvalEvent {
                recorded_at_unix: 2,
                ..event.clone()
            })
            .expect("encode")
                + "\n",
        )
        .expect("current events");

        let loaded = load_events_from_paths(
            vec![
                legacy_path.join("events.jsonl"),
                current_path.join("events.jsonl"),
            ],
            None,
            10,
        )
        .expect("load");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].recorded_at_unix, 2);
    }

    #[test]
    fn load_events_sanitizes_contextual_queries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let event = CodexSkillEvalEvent {
            version: 1,
            recorded_at_unix: 1,
            mode: "quick-launch".to_string(),
            action: "resume".to_string(),
            route: "quick-launch-hydrated".to_string(),
            target_path: "/tmp/repo".to_string(),
            launch_path: "/tmp/repo".to_string(),
            query: "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>\n<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>\nwrite plan".to_string(),
            session_id: Some("sess-1".to_string()),
            runtime_token: None,
            runtime_skills: Vec::new(),
            prompt_context_budget_chars: 0,
            prompt_chars: 10,
            injected_context_chars: 0,
            reference_count: 0,
        };
        fs::write(&path, serde_json::to_string(&event).expect("encode") + "\n").expect("write");

        let loaded = load_events_from_paths(vec![path], None, 10).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].query, "write plan");
    }

    #[test]
    fn resolve_events_contribute_outcome_samples() {
        let target = Path::new("/tmp/repo");
        let scorecard = build_scorecard(
            target,
            vec![CodexSkillEvalEvent {
                version: 1,
                recorded_at_unix: 1,
                mode: "resolve".to_string(),
                action: "new".to_string(),
                route: "new-plain".to_string(),
                target_path: target.display().to_string(),
                launch_path: target.display().to_string(),
                query: "write plan".to_string(),
                session_id: None,
                runtime_token: Some("tok".to_string()),
                runtime_skills: vec!["plan_write".to_string()],
                prompt_context_budget_chars: 400,
                prompt_chars: 100,
                injected_context_chars: 30,
                reference_count: 0,
            }],
            vec![CodexSkillOutcomeEvent {
                version: 1,
                recorded_at_unix: 2,
                runtime_token: Some("tok".to_string()),
                session_id: None,
                target_path: Some(target.display().to_string()),
                kind: "plan_written".to_string(),
                skill_names: vec!["plan_write".to_string()],
                artifact_path: Some("/tmp/repo/plan.md".to_string()),
                success: 1.0,
            }],
        );

        assert_eq!(scorecard.samples, 1);
        assert_eq!(scorecard.skills.len(), 1);
        assert_eq!(scorecard.skills[0].name, "plan_write");
        assert_eq!(scorecard.skills[0].outcome_samples, 1);
        assert_eq!(scorecard.skills[0].pass_rate, 1.0);
    }

    #[test]
    fn session_linked_events_contribute_baseline_outcomes() {
        let target = Path::new("/tmp/repo");
        let scorecard = build_scorecard(
            target,
            vec![CodexSkillEvalEvent {
                version: 1,
                recorded_at_unix: 1,
                mode: "quick-launch".to_string(),
                action: "resume".to_string(),
                route: "quick-launch-hydrated".to_string(),
                target_path: target.display().to_string(),
                launch_path: target.display().to_string(),
                query: "write plan".to_string(),
                session_id: Some("sess-1".to_string()),
                runtime_token: None,
                runtime_skills: vec!["plan_write".to_string()],
                prompt_context_budget_chars: 0,
                prompt_chars: 10,
                injected_context_chars: 0,
                reference_count: 0,
            }],
            vec![CodexSkillOutcomeEvent {
                version: 1,
                recorded_at_unix: 2,
                runtime_token: None,
                session_id: Some("sess-1".to_string()),
                target_path: Some(target.display().to_string()),
                kind: "plan_written".to_string(),
                skill_names: vec!["plan_write".to_string()],
                artifact_path: Some("/tmp/repo/plan.md".to_string()),
                success: 1.0,
            }],
        );

        assert_eq!(scorecard.skills.len(), 1);
        assert_eq!(scorecard.skills[0].outcome_samples, 1);
        assert_eq!(scorecard.skills[0].pass_rate, 1.0);
    }

    #[test]
    fn recent_targets_filters_old_missing_and_excess_targets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_a = dir.path().join("repo-a");
        let repo_b = dir.path().join("repo-b");
        fs::create_dir_all(&repo_a).expect("repo a");
        fs::create_dir_all(&repo_b).expect("repo b");
        let now = unix_now();

        let targets = collect_recent_targets(
            vec![
                CodexSkillEvalEvent {
                    version: 1,
                    recorded_at_unix: now,
                    mode: "resolve".to_string(),
                    action: "new".to_string(),
                    route: "new-plain".to_string(),
                    target_path: repo_a.display().to_string(),
                    launch_path: repo_a.display().to_string(),
                    query: "write plan".to_string(),
                    session_id: None,
                    runtime_token: Some("a".to_string()),
                    runtime_skills: vec!["plan_write".to_string()],
                    prompt_context_budget_chars: 400,
                    prompt_chars: 100,
                    injected_context_chars: 30,
                    reference_count: 0,
                },
                CodexSkillEvalEvent {
                    version: 1,
                    recorded_at_unix: now.saturating_sub(60),
                    mode: "resolve".to_string(),
                    action: "new".to_string(),
                    route: "new-plain".to_string(),
                    target_path: repo_b.display().to_string(),
                    launch_path: repo_b.display().to_string(),
                    query: "find skills".to_string(),
                    session_id: None,
                    runtime_token: Some("b".to_string()),
                    runtime_skills: vec!["find-skills".to_string()],
                    prompt_context_budget_chars: 400,
                    prompt_chars: 100,
                    injected_context_chars: 30,
                    reference_count: 0,
                },
                CodexSkillEvalEvent {
                    version: 1,
                    recorded_at_unix: now.saturating_sub(60 * 60 * 24 * 10),
                    mode: "resolve".to_string(),
                    action: "new".to_string(),
                    route: "new-plain".to_string(),
                    target_path: dir.path().join("missing").display().to_string(),
                    launch_path: dir.path().join("missing").display().to_string(),
                    query: "old".to_string(),
                    session_id: None,
                    runtime_token: Some("c".to_string()),
                    runtime_skills: vec!["plan_write".to_string()],
                    prompt_context_budget_chars: 400,
                    prompt_chars: 100,
                    injected_context_chars: 30,
                    reference_count: 0,
                },
            ],
            1,
            24,
        );

        assert_eq!(targets, vec![repo_a]);
    }
}
