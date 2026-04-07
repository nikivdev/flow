use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::{config, project_snapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRefUpdate {
    pub local_ref: String,
    pub local_sha: String,
    pub remote_ref: String,
    pub remote_sha: String,
}

#[derive(Debug, Clone)]
pub struct PushContext {
    pub repo_root: PathBuf,
    pub current_branch: Option<String>,
    pub remote_name: Option<String>,
    pub remote_url: Option<String>,
    pub updates: Vec<PushRefUpdate>,
    pub orchestrated: bool,
}

#[derive(Debug, Clone)]
pub struct EffectivePushPolicyConfig {
    pub enabled: bool,
    pub hooks_path: PathBuf,
    pub prek_bin: Option<PathBuf>,
    pub default_mode: config::PushPolicyMode,
    pub repos: Vec<EffectivePushRepoRule>,
}

#[derive(Debug, Clone)]
pub struct EffectivePushRepoRule {
    pub match_label: String,
    pub match_path: PathBuf,
    pub mode: Option<config::PushPolicyMode>,
    pub home_branch: Option<String>,
    pub run_prek: bool,
    pub mirror_main: bool,
    pub remote_name: Option<String>,
    pub remote_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPushPolicy {
    pub mode: config::PushPolicyMode,
    pub matched_rule: Option<String>,
    pub home_branch: Option<String>,
    pub run_prek: bool,
    pub mirror_main: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushDecision {
    pub allow: bool,
    pub message: Option<String>,
    pub policy: ResolvedPushPolicy,
}

#[derive(Debug, Clone, Default)]
struct PushPolicyMergeState {
    enabled: Option<bool>,
    hooks_path: Option<String>,
    prek_bin: Option<String>,
    default_mode: Option<config::PushPolicyMode>,
    repos: Vec<EffectivePushRepoRule>,
}

pub fn default_hooks_path() -> PathBuf {
    config::global_config_dir().join("git-hooks")
}

pub fn effective_global_hooks_path() -> Result<PathBuf> {
    let global_path = config::default_config_path();
    if !global_path.exists() {
        return Ok(default_hooks_path());
    }

    let cfg = config::load(&global_path)?;
    Ok(cfg
        .push_policy
        .and_then(|policy| policy.hooks_path)
        .map(|value| config::expand_path(&value))
        .unwrap_or_else(default_hooks_path))
}

pub fn load_merged_push_policy(repo_root: &Path) -> Result<EffectivePushPolicyConfig> {
    let mut merged = PushPolicyMergeState::default();

    let global_path = config::default_config_path();
    if global_path.exists() {
        let global_cfg = config::load(&global_path)?;
        if let Some(policy) = global_cfg.push_policy {
            merge_push_policy(
                &mut merged,
                policy,
                global_path.parent().unwrap_or_else(|| Path::new(".")),
            );
        }
    }

    if let Some(local_path) = project_snapshot::find_flow_toml_upwards(repo_root)
        && local_path.exists()
    {
        let local_cfg = config::load(&local_path)?;
        if let Some(policy) = local_cfg.push_policy {
            merge_push_policy(
                &mut merged,
                policy,
                local_path.parent().unwrap_or_else(|| Path::new(".")),
            );
        }
    }

    Ok(EffectivePushPolicyConfig {
        enabled: merged.enabled.unwrap_or(false),
        hooks_path: merged
            .hooks_path
            .map(|value| config::expand_path(&value))
            .unwrap_or_else(default_hooks_path),
        prek_bin: merged.prek_bin.map(|value| config::expand_path(&value)),
        default_mode: merged
            .default_mode
            .unwrap_or(config::PushPolicyMode::Passthrough),
        repos: merged.repos,
    })
}

pub fn evaluate_pre_push(ctx: &PushContext, config: &EffectivePushPolicyConfig) -> PushDecision {
    let resolved = resolve_push_policy(ctx, config);
    match resolved.mode {
        config::PushPolicyMode::Disabled | config::PushPolicyMode::Passthrough => PushDecision {
            allow: true,
            message: None,
            policy: resolved,
        },
        config::PushPolicyMode::ValidateOnly => PushDecision {
            allow: true,
            message: None,
            policy: resolved,
        },
        config::PushPolicyMode::RequireFlowPush => {
            if ctx.orchestrated {
                return PushDecision {
                    allow: true,
                    message: None,
                    policy: resolved,
                };
            }
            let rule = resolved
                .matched_rule
                .clone()
                .unwrap_or_else(|| ctx.repo_root.display().to_string());
            PushDecision {
                allow: false,
                message: Some(format!(
                    "Push policy for {rule} requires Flow orchestration. Use `f push`."
                )),
                policy: resolved,
            }
        }
        config::PushPolicyMode::HomeBranchOnly => {
            let Some(home_branch) = resolved.home_branch.clone() else {
                return PushDecision {
                    allow: false,
                    message: Some(
                        "Push policy requires `home_branch` to be configured for home_branch_only mode."
                            .to_string(),
                    ),
                    policy: resolved,
                };
            };

            let pushed_branches = pushed_branch_names(&ctx.updates);
            if pushed_branches
                .iter()
                .any(|branch| branch.as_str() != home_branch)
            {
                let violating = pushed_branches
                    .into_iter()
                    .find(|branch| branch.as_str() != home_branch)
                    .unwrap_or_else(|| "<unknown>".to_string());
                return PushDecision {
                    allow: false,
                    message: Some(format!(
                        "Push policy only allows branch pushes from `{home_branch}`; refusing `{violating}`."
                    )),
                    policy: resolved,
                };
            }

            if let Some(current_branch) = ctx.current_branch.as_deref()
                && current_branch != home_branch
                && !ctx.updates.is_empty()
            {
                return PushDecision {
                    allow: false,
                    message: Some(format!(
                        "Push policy only allows pushes from home branch `{home_branch}`; current branch is `{current_branch}`."
                    )),
                    policy: resolved,
                };
            }

            PushDecision {
                allow: true,
                message: None,
                policy: resolved,
            }
        }
    }
}

pub fn parse_pre_push_updates(input: &str) -> Vec<PushRefUpdate> {
    input
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let mut parts = trimmed.split_whitespace();
            Some(PushRefUpdate {
                local_ref: parts.next()?.to_string(),
                local_sha: parts.next()?.to_string(),
                remote_ref: parts.next()?.to_string(),
                remote_sha: parts.next()?.to_string(),
            })
        })
        .collect()
}

fn merge_push_policy(
    target: &mut PushPolicyMergeState,
    overlay: config::PushPolicyConfig,
    base_dir: &Path,
) {
    if let Some(enabled) = overlay.enabled {
        target.enabled = Some(enabled);
    }
    if let Some(hooks_path) = overlay.hooks_path {
        target.hooks_path = Some(hooks_path);
    }
    if let Some(prek_bin) = overlay.prek_bin {
        target.prek_bin = Some(prek_bin);
    }
    if let Some(default_mode) = overlay.default_mode {
        target.default_mode = Some(default_mode);
    }
    target.repos.extend(overlay.repos.into_iter().map(|rule| {
        let match_path = resolve_rule_match_path(&rule.match_path, base_dir);
        EffectivePushRepoRule {
            match_label: rule.match_path,
            match_path,
            mode: rule.mode,
            home_branch: rule.home_branch,
            run_prek: rule.run_prek,
            mirror_main: rule.mirror_main,
            remote_name: rule.remote_name,
            remote_url: rule.remote_url,
        }
    }));
}

fn resolve_rule_match_path(value: &str, base_dir: &Path) -> PathBuf {
    let expanded = config::expand_path(value);
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

fn resolve_push_policy(
    ctx: &PushContext,
    config: &EffectivePushPolicyConfig,
) -> ResolvedPushPolicy {
    if !config.enabled {
        return ResolvedPushPolicy {
            mode: config::PushPolicyMode::Passthrough,
            matched_rule: None,
            home_branch: None,
            run_prek: false,
            mirror_main: false,
        };
    }

    let matched_rule = config
        .repos
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule_matches(rule, ctx))
        .max_by_key(|(index, rule)| {
            (
                path_specificity(&rule.match_path),
                rule.remote_name.is_some(),
                rule.remote_url.is_some(),
                *index,
            )
        })
        .map(|(_, rule)| rule);

    ResolvedPushPolicy {
        mode: matched_rule
            .and_then(|rule| rule.mode)
            .unwrap_or(config.default_mode),
        matched_rule: matched_rule.map(|rule| rule.match_label.clone()),
        home_branch: matched_rule.and_then(|rule| rule.home_branch.clone()),
        run_prek: matched_rule.map(|rule| rule.run_prek).unwrap_or(false),
        mirror_main: matched_rule.map(|rule| rule.mirror_main).unwrap_or(false),
    }
}

fn rule_matches(rule: &EffectivePushRepoRule, ctx: &PushContext) -> bool {
    if !(ctx.repo_root == rule.match_path || ctx.repo_root.starts_with(&rule.match_path)) {
        return false;
    }

    if let Some(expected_name) = rule.remote_name.as_deref()
        && ctx.remote_name.as_deref() != Some(expected_name)
    {
        return false;
    }

    if let Some(expected_url) = rule.remote_url.as_deref() {
        let Some(actual_url) = ctx.remote_url.as_deref() else {
            return false;
        };
        if !remote_urls_match(expected_url, actual_url) {
            return false;
        }
    }

    true
}

fn path_specificity(path: &Path) -> usize {
    path.components().count()
}

fn remote_urls_match(expected: &str, actual: &str) -> bool {
    normalize_remote_url(expected) == normalize_remote_url(actual)
}

fn normalize_remote_url(value: &str) -> String {
    value
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

fn pushed_branch_names(updates: &[PushRefUpdate]) -> Vec<String> {
    updates
        .iter()
        .filter_map(|update| branch_name_from_ref(&update.local_ref))
        .map(ToString::to_string)
        .collect()
}

fn branch_name_from_ref(reference: &str) -> Option<&str> {
    reference.strip_prefix("refs/heads/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pre_push_updates_reads_valid_lines_only() {
        let updates = parse_pre_push_updates(
            "refs/heads/nikiv abc refs/heads/nikiv def\n\ninvalid\nrefs/tags/v1 aaa refs/tags/v1 bbb\n",
        );

        assert_eq!(
            updates,
            vec![
                PushRefUpdate {
                    local_ref: "refs/heads/nikiv".to_string(),
                    local_sha: "abc".to_string(),
                    remote_ref: "refs/heads/nikiv".to_string(),
                    remote_sha: "def".to_string(),
                },
                PushRefUpdate {
                    local_ref: "refs/tags/v1".to_string(),
                    local_sha: "aaa".to_string(),
                    remote_ref: "refs/tags/v1".to_string(),
                    remote_sha: "bbb".to_string(),
                },
            ]
        );
    }

    #[test]
    fn home_branch_only_denies_non_home_branch_push() {
        let ctx = PushContext {
            repo_root: PathBuf::from("/tmp/repo"),
            current_branch: Some("feature".to_string()),
            remote_name: Some("origin".to_string()),
            remote_url: None,
            updates: vec![PushRefUpdate {
                local_ref: "refs/heads/feature".to_string(),
                local_sha: "abc".to_string(),
                remote_ref: "refs/heads/feature".to_string(),
                remote_sha: "def".to_string(),
            }],
            orchestrated: false,
        };
        let config = EffectivePushPolicyConfig {
            enabled: true,
            hooks_path: default_hooks_path(),
            prek_bin: None,
            default_mode: config::PushPolicyMode::Passthrough,
            repos: vec![EffectivePushRepoRule {
                match_label: "/tmp/repo".to_string(),
                match_path: PathBuf::from("/tmp/repo"),
                mode: Some(config::PushPolicyMode::HomeBranchOnly),
                home_branch: Some("nikiv".to_string()),
                run_prek: false,
                mirror_main: false,
                remote_name: None,
                remote_url: None,
            }],
        };

        let decision = evaluate_pre_push(&ctx, &config);
        assert!(!decision.allow);
        let message = decision.message.as_deref().unwrap_or_default();
        assert!(message.contains("nikiv"));
        assert!(message.contains("feature"));
    }

    #[test]
    fn require_flow_push_denies_raw_push() {
        let ctx = PushContext {
            repo_root: PathBuf::from("/tmp/repo"),
            current_branch: Some("nikiv".to_string()),
            remote_name: Some("origin".to_string()),
            remote_url: None,
            updates: vec![PushRefUpdate {
                local_ref: "refs/heads/nikiv".to_string(),
                local_sha: "abc".to_string(),
                remote_ref: "refs/heads/nikiv".to_string(),
                remote_sha: "def".to_string(),
            }],
            orchestrated: false,
        };
        let config = EffectivePushPolicyConfig {
            enabled: true,
            hooks_path: default_hooks_path(),
            prek_bin: None,
            default_mode: config::PushPolicyMode::Passthrough,
            repos: vec![EffectivePushRepoRule {
                match_label: "/tmp/repo".to_string(),
                match_path: PathBuf::from("/tmp/repo"),
                mode: Some(config::PushPolicyMode::RequireFlowPush),
                home_branch: Some("nikiv".to_string()),
                run_prek: false,
                mirror_main: true,
                remote_name: None,
                remote_url: None,
            }],
        };

        let decision = evaluate_pre_push(&ctx, &config);
        assert!(!decision.allow);
        assert_eq!(
            decision.policy.mode,
            config::PushPolicyMode::RequireFlowPush
        );
    }

    #[test]
    fn more_specific_rule_wins() {
        let ctx = PushContext {
            repo_root: PathBuf::from("/tmp/repos/zed-industries/zed"),
            current_branch: Some("nikiv".to_string()),
            remote_name: Some("origin".to_string()),
            remote_url: None,
            updates: Vec::new(),
            orchestrated: false,
        };
        let config = EffectivePushPolicyConfig {
            enabled: true,
            hooks_path: default_hooks_path(),
            prek_bin: None,
            default_mode: config::PushPolicyMode::Passthrough,
            repos: vec![
                EffectivePushRepoRule {
                    match_label: "/tmp/repos".to_string(),
                    match_path: PathBuf::from("/tmp/repos"),
                    mode: Some(config::PushPolicyMode::Passthrough),
                    home_branch: None,
                    run_prek: false,
                    mirror_main: false,
                    remote_name: None,
                    remote_url: None,
                },
                EffectivePushRepoRule {
                    match_label: "/tmp/repos/zed-industries/zed".to_string(),
                    match_path: PathBuf::from("/tmp/repos/zed-industries/zed"),
                    mode: Some(config::PushPolicyMode::RequireFlowPush),
                    home_branch: Some("nikiv".to_string()),
                    run_prek: false,
                    mirror_main: true,
                    remote_name: None,
                    remote_url: None,
                },
            ],
        };

        let decision = evaluate_pre_push(&ctx, &config);
        assert_eq!(
            decision.policy.mode,
            config::PushPolicyMode::RequireFlowPush
        );
        assert_eq!(
            decision.policy.matched_rule.as_deref(),
            Some("/tmp/repos/zed-industries/zed")
        );
    }
}
