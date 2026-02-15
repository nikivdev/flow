//! Branch discovery and selection utilities.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::ai_server;
use crate::cli::{
    BranchAiOpts, BranchFindOpts, BranchListOpts, BranchesAction, BranchesCommand, SwitchCommand,
};
use crate::sync;

#[derive(Debug, Clone)]
struct BranchEntry {
    name: String,
    subject: String,
    upstream: Option<String>,
    is_remote: bool,
}

pub fn run(cmd: BranchesCommand) -> Result<()> {
    match cmd.action {
        Some(BranchesAction::List(opts)) => run_list(opts),
        Some(BranchesAction::Find(opts)) => run_find(opts),
        Some(BranchesAction::Ai(opts)) => run_ai(opts),
        None => run_list(BranchListOpts {
            remote: false,
            limit: 40,
        }),
    }
}

fn run_list(opts: BranchListOpts) -> Result<()> {
    let branches = collect_branches(opts.remote)?;
    if branches.is_empty() {
        println!("No branches found.");
        return Ok(());
    }

    let limit = opts.limit.max(1);
    for entry in branches.iter().take(limit) {
        print_branch(entry);
    }
    Ok(())
}

fn run_find(opts: BranchFindOpts) -> Result<()> {
    let query = opts.query.trim().to_string();
    if query.is_empty() {
        bail!("Query cannot be empty");
    }

    let branches = collect_branches(opts.remote)?;
    if branches.is_empty() {
        bail!("No branches available to search");
    }

    let ranked = rank_branches(&query, &branches);
    if ranked.is_empty() {
        bail!("No branches matched query '{}'.", query);
    }

    let limit = opts.limit.max(1);
    for (_, entry) in ranked.iter().take(limit) {
        print_branch(entry);
    }

    if opts.switch {
        let best = ranked
            .first()
            .map(|(_, entry)| (*entry).clone())
            .context("No match available to switch")?;
        println!("\nSwitching to {}...", best.name);
        switch_to_entry(&best)?;
    }

    Ok(())
}

fn run_ai(opts: BranchAiOpts) -> Result<()> {
    let query = opts.query.trim().to_string();
    if query.is_empty() {
        bail!("Query cannot be empty");
    }

    let branches = collect_branches(opts.remote)?;
    if branches.is_empty() {
        bail!("No branches available for AI matching");
    }

    let candidates = top_candidates_for_ai(&query, &branches, opts.limit.max(1));
    let prompt = build_ai_prompt(&query, &candidates);
    let response =
        ai_server::quick_prompt(&prompt, opts.model.as_deref(), opts.url.as_deref(), None)?;
    let selected_name = parse_ai_branch_response(&response, &candidates)
        .with_context(|| format!("Could not parse AI branch response: {}", response.trim()))?;

    let selected = candidates
        .iter()
        .find(|e| e.name == selected_name)
        .cloned()
        .context("AI selected branch that is not in candidate list")?;

    println!("Selected branch:");
    print_branch(&selected);

    if opts.switch {
        println!("\nSwitching to {}...", selected.name);
        switch_to_entry(&selected)?;
    }

    Ok(())
}

fn print_branch(entry: &BranchEntry) {
    let mut line = if entry.is_remote {
        format!("{} [remote]", entry.name)
    } else {
        entry.name.clone()
    };

    if let Some(upstream) = entry.upstream.as_deref() {
        if !upstream.is_empty() {
            line.push_str(&format!(" -> {}", upstream));
        }
    }

    if !entry.subject.is_empty() {
        line.push_str(&format!(" :: {}", entry.subject));
    }

    println!("{}", line);
}

fn collect_branches(include_remote: bool) -> Result<Vec<BranchEntry>> {
    let mut out = collect_local_branches()?;

    if include_remote {
        out.extend(collect_remote_branches()?);
    }

    Ok(out)
}

fn collect_local_branches() -> Result<Vec<BranchEntry>> {
    let raw = git_capture(&[
        "for-each-ref",
        "--sort=-committerdate",
        "--format=%(refname:short)%00%(upstream:short)%00%(subject)",
        "refs/heads",
    ])?;

    let mut branches = Vec::new();
    for line in raw.lines() {
        let mut parts = line.split('\0');
        let name = parts.next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let upstream = parts.next().unwrap_or("").trim().to_string();
        let subject = parts.next().unwrap_or("").trim().to_string();

        branches.push(BranchEntry {
            name: name.to_string(),
            subject,
            upstream: if upstream.is_empty() {
                None
            } else {
                Some(upstream)
            },
            is_remote: false,
        });
    }

    Ok(branches)
}

fn collect_remote_branches() -> Result<Vec<BranchEntry>> {
    let raw = git_capture(&[
        "for-each-ref",
        "--sort=-committerdate",
        "--format=%(refname:short)%00%(subject)",
        "refs/remotes",
    ])?;

    let mut branches = Vec::new();
    let mut seen = HashSet::new();
    for line in raw.lines() {
        let mut parts = line.split('\0');
        let name = parts.next().unwrap_or("").trim();
        if name.is_empty() || name.ends_with("/HEAD") {
            continue;
        }
        if !seen.insert(name.to_string()) {
            continue;
        }

        let subject = parts.next().unwrap_or("").trim().to_string();
        branches.push(BranchEntry {
            name: name.to_string(),
            subject,
            upstream: None,
            is_remote: true,
        });
    }

    Ok(branches)
}

fn rank_branches<'a>(query: &str, branches: &'a [BranchEntry]) -> Vec<(i64, &'a BranchEntry)> {
    let q = query.to_ascii_lowercase();
    let tokens: Vec<&str> = q.split_whitespace().filter(|t| !t.is_empty()).collect();

    let mut ranked = Vec::new();
    for (idx, entry) in branches.iter().enumerate() {
        let hay_name = entry.name.to_ascii_lowercase();
        let hay_subject = entry.subject.to_ascii_lowercase();

        let mut score: i64 = 0;
        let mut matched = false;

        if let Some(pos) = hay_name.find(&q) {
            matched = true;
            score += 10_000 - pos as i64;
        }
        if let Some(pos) = hay_subject.find(&q) {
            matched = true;
            score += 3_000 - pos as i64;
        }

        let mut all_tokens_match = true;
        for token in &tokens {
            if hay_name.contains(token) {
                score += 700;
            } else if hay_subject.contains(token) {
                score += 250;
            } else {
                all_tokens_match = false;
            }
        }

        if !tokens.is_empty() && all_tokens_match {
            matched = true;
            score += 1_500;
        }

        if !matched {
            continue;
        }

        // Stable tie-break using recency order from git listing (earlier index first).
        score -= idx as i64;
        ranked.push((score, entry));
    }

    ranked.sort_by(|a, b| match b.0.cmp(&a.0) {
        Ordering::Equal => a.1.name.cmp(&b.1.name),
        other => other,
    });

    ranked
}

fn top_candidates_for_ai(query: &str, branches: &[BranchEntry], limit: usize) -> Vec<BranchEntry> {
    let mut candidates: Vec<BranchEntry> = rank_branches(query, branches)
        .into_iter()
        .map(|(_, entry)| (*entry).clone())
        .take(limit)
        .collect();

    if candidates.is_empty() {
        candidates = branches.iter().take(limit).cloned().collect();
    }

    candidates
}

fn build_ai_prompt(query: &str, candidates: &[BranchEntry]) -> String {
    let mut prompt = String::new();
    prompt.push_str("You are selecting a git branch for a user query.\\n");
    prompt.push_str("Return exactly one line in one of these formats:\\n");
    prompt.push_str("branch:<exact branch name>\\n");
    prompt.push_str("none\\n\\n");
    prompt.push_str("Candidate branches:\\n");

    for entry in candidates {
        let remote = if entry.is_remote { "remote" } else { "local" };
        prompt.push_str(&format!(
            "- {} [{}] :: {}\\n",
            entry.name,
            remote,
            if entry.subject.is_empty() {
                "(no subject)"
            } else {
                &entry.subject
            }
        ));
    }

    prompt.push_str(&format!("\\nUser query: {}\\n", query));
    prompt.push_str("Answer:");

    prompt
}

fn parse_ai_branch_response(response: &str, candidates: &[BranchEntry]) -> Option<String> {
    let cleaned = response.trim().trim_matches('`').trim();
    if cleaned.eq_ignore_ascii_case("none") {
        return None;
    }

    if let Some(name) = cleaned.strip_prefix("branch:") {
        let selected = name.trim();
        if candidates.iter().any(|c| c.name == selected) {
            return Some(selected.to_string());
        }
    }

    // Fallback: accept exact branch name response.
    if candidates.iter().any(|c| c.name == cleaned) {
        return Some(cleaned.to_string());
    }

    None
}

fn switch_to_entry(entry: &BranchEntry) -> Result<()> {
    if entry.is_remote {
        let (remote, branch) = entry
            .name
            .split_once('/')
            .context("Remote branch name is malformed")?;
        sync::run_switch(SwitchCommand {
            branch: branch.to_string(),
            remote: Some(remote.to_string()),
            preserve: true,
            no_preserve: false,
            stash: true,
            no_stash: false,
            sync: false,
        })?;
    } else {
        sync::run_switch(SwitchCommand {
            branch: entry.name.clone(),
            remote: None,
            preserve: true,
            no_preserve: false,
            stash: true,
            no_stash: false,
            sync: false,
        })?;
    }

    Ok(())
}

fn git_capture(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
