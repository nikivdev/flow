use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use regex::Regex;

pub type SecretFinding = (String, usize, String, String);

/// Common secret patterns to detect in diff content.
/// Each tuple is (pattern_name, regex_pattern).
const SECRET_PATTERNS: &[(&str, &str)] = &[
    // API Keys with known prefixes
    ("AWS Access Key", r"AKIA[0-9A-Z]{16}"),
    (
        "AWS Secret Key",
        r#"(?i)aws.{0,20}secret.{0,20}['"][0-9a-zA-Z/+]{40}['"]"#,
    ),
    ("GitHub Token", r"ghp_[0-9a-zA-Z]{36}"),
    ("GitHub OAuth", r"gho_[0-9a-zA-Z]{36}"),
    ("GitHub App Token", r"ghu_[0-9a-zA-Z]{36}"),
    ("GitHub Refresh Token", r"ghr_[0-9a-zA-Z]{36}"),
    ("GitLab Token", r"glpat-[0-9a-zA-Z\\-_]{20,}"),
    ("Slack Token", r"xox[baprs]-[0-9a-zA-Z]{10,48}"),
    (
        "Slack Webhook",
        r"https://hooks\.slack\.com/services/T[0-9A-Z]{8,}/B[0-9A-Z]{8,}/[0-9a-zA-Z]{24}",
    ),
    (
        "Discord Webhook",
        r"https://discord(?:app)?\.com/api/webhooks/[0-9]{17,}/[0-9a-zA-Z_-]{60,}",
    ),
    ("Stripe Key", r"sk_live_[0-9a-zA-Z]{24,}"),
    ("Stripe Restricted", r"rk_live_[0-9a-zA-Z]{24,}"),
    // OpenAI keys - multiple formats (legacy, project, service account)
    ("OpenAI Key (Legacy)", r"sk-[a-zA-Z0-9]{32,}"),
    ("OpenAI Key (Project)", r"sk-proj-[a-zA-Z0-9\\-_]{20,}"),
    ("OpenAI Key (Service)", r"sk-svcacct-[a-zA-Z0-9\\-_]{20,}"),
    ("Anthropic Key", r"sk-ant-[0-9a-zA-Z\\-_]{90,}"),
    ("Google API Key", r"AIza[0-9A-Za-z\\-_]{35}"),
    ("Groq API Key", r"gsk_[0-9a-zA-Z]{50,}"),
    (
        "Mistral API Key",
        r#"(?i)mistral.{0,10}(api[_-]?key|key).{0,5}[=:].{0,5}["'][0-9a-zA-Z]{32,}["']"#,
    ),
    (
        "Cohere API Key",
        r#"(?i)cohere.{0,10}(api[_-]?key|key).{0,5}[=:].{0,5}["'][0-9a-zA-Z]{40,}["']"#,
    ),
    (
        "Heroku API Key",
        r"(?i)heroku.{0,20}[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
    ),
    ("NPM Token", r"npm_[0-9a-zA-Z]{36}"),
    ("PyPI Token", r"pypi-[0-9a-zA-Z_-]{50,}"),
    ("Telegram Bot Token", r"[0-9]{8,10}:[0-9A-Za-z_-]{35}"),
    ("Twilio Key", r"SK[0-9a-fA-F]{32}"),
    ("SendGrid Key", r"SG\.[0-9a-zA-Z_-]{22}\.[0-9a-zA-Z_-]{43}"),
    ("Mailgun Key", r"key-[0-9a-zA-Z]{32}"),
    (
        "Private Key",
        r"-----BEGIN (RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----",
    ),
    (
        "Supabase Key",
        r"eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9\.[0-9a-zA-Z_-]{50,}",
    ),
    (
        "Firebase Key",
        r#"(?i)firebase.{0,20}["'][A-Za-z0-9_-]{30,}["']"#,
    ),
    // Generic patterns (higher false positive risk, but catch common mistakes)
    (
        "Generic API Key Assignment",
        r#"(?i)(api[_-]?key|apikey)\s*[:=]\s*['"][0-9a-zA-Z\-_]{20,}['"]"#,
    ),
    (
        "Generic Secret Assignment",
        r#"(?i)(secret|password|passwd|pwd)\s*[:=]\s*['"][^'"]{8,}['"]"#,
    ),
    ("Bearer Token", r"(?i)bearer\s+[0-9a-zA-Z\-_.]{20,}"),
    ("Basic Auth", r"(?i)basic\s+[A-Za-z0-9+/=]{20,}"),
    // High-entropy strings that look like secrets (env var assignments)
    (
        "Env Var Secret",
        r#"(?i)(KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL|AUTH)[_A-Z]*\s*=\s*['"]?[0-9a-zA-Z\-_/+=]{32,}['"]?"#,
    ),
];

fn compiled_secret_patterns() -> &'static Vec<(&'static str, Regex)> {
    static COMPILED: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        SECRET_PATTERNS
            .iter()
            .filter_map(|(name, pattern)| Regex::new(pattern).ok().map(|re| (*name, re)))
            .collect()
    })
}

const SECRET_SCAN_IGNORE_MARKERS: &[&str] = &[
    "flow:secret:ignore",
    "flow-secret-ignore",
    "flow:secret-scan:ignore",
    "gitleaks:allow",
];

fn should_ignore_secret_scan_line(content: &str) -> bool {
    let lower = content.to_lowercase();
    SECRET_SCAN_IGNORE_MARKERS
        .iter()
        .any(|m| lower.contains(&m.to_lowercase()))
}

fn extract_first_quoted_value(s: &str) -> Option<&str> {
    let (qpos, qch) = s.char_indices().find(|(_, c)| *c == '"' || *c == '\'')?;
    let end = s.rfind(qch)?;
    if end <= qpos {
        return None;
    }
    Some(&s[qpos + 1..end])
}

fn looks_like_identifier_reference(value: &str) -> bool {
    let v = value.trim();
    !v.is_empty()
        && v.len() >= 8
        && v.contains('_')
        && v.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '.')
}

fn looks_like_secret_lookup(value: &str) -> bool {
    let v = value.trim();

    if v.starts_with("${") && v.ends_with('}') {
        let inner = &v[2..v.len() - 1];
        return !inner.contains(":-")
            && !inner.contains("-")
            && inner
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    }

    if !(v.starts_with("$(") && v.ends_with(')')) {
        return false;
    }
    let inner = v[2..v.len() - 1].trim();
    if inner.contains('"') || inner.contains('\'') || inner.contains('`') {
        return false;
    }
    let inner_lc = inner.to_lowercase();
    inner_lc.starts_with("get_env ")
        || inner_lc.starts_with("getenv ")
        || inner_lc.starts_with("printenv ")
        || inner_lc.starts_with("op read ")
        || inner_lc.starts_with("pass show ")
        || inner_lc.starts_with("security find-generic-password")
        || inner_lc.starts_with("aws ssm get-parameter")
        || inner_lc.starts_with("vault kv get")
        || inner_lc.starts_with("bw get")
        || inner_lc.starts_with("gcloud secrets versions access")
}

fn generic_secret_assignment_is_false_positive(content: &str, matched: &str) -> bool {
    if let Some((_, rhs)) = matched.split_once('=') {
        let rhs = rhs.trim_start();
        if rhs.starts_with("\"$(") || rhs.starts_with("'$(") || rhs.starts_with("`") {
            return true;
        }
        if rhs.starts_with("\"$") || rhs.starts_with("'$") {
            return true;
        }
    } else if let Some((_, rhs)) = matched.split_once(':') {
        let rhs = rhs.trim_start();
        if rhs.starts_with("\"$(") || rhs.starts_with("'$(") || rhs.starts_with("`") {
            return true;
        }
        if rhs.starts_with("\"$") || rhs.starts_with("'$") {
            return true;
        }
    }

    if let Some(val) = extract_first_quoted_value(matched) {
        let v = val.trim();
        if looks_like_identifier_reference(v) {
            return true;
        }
        if looks_like_secret_lookup(v) {
            return true;
        }
    }

    let lc = content.to_lowercase();
    lc.contains("$(get_env ")
}

/// Scan staged diff content for hardcoded secrets.
/// Returns list of (file, line_num, pattern_name, matched_text) for detected secrets.
pub fn scan_diff_for_secrets(repo_root: &Path) -> Vec<SecretFinding> {
    let output = Command::new("git")
        .args(["diff", "--cached", "-U0"])
        .current_dir(repo_root)
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    let diff = String::from_utf8_lossy(&output.stdout);
    let mut findings: Vec<SecretFinding> = Vec::new();
    let mut current_file = String::new();
    let mut current_line: usize = 0;
    let mut ignore_next_added_line = false;

    let patterns = compiled_secret_patterns();

    for line in diff.lines() {
        if line.starts_with("+++ b/") {
            current_file = line.strip_prefix("+++ b/").unwrap_or("").to_string();
            ignore_next_added_line = false;
            continue;
        }

        if line.starts_with("@@") {
            if let Some(plus_pos) = line.find('+') {
                let after_plus = &line[plus_pos + 1..];
                let num_str: String = after_plus
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                current_line = num_str.parse().unwrap_or(0);
            }
            ignore_next_added_line = false;
            continue;
        }

        if line.starts_with('+') && !line.starts_with("+++") {
            let content = &line[1..];

            if ignore_next_added_line {
                ignore_next_added_line = false;
                current_line += 1;
                continue;
            }
            let trimmed = content.trim_start();
            if trimmed.starts_with('#') && should_ignore_secret_scan_line(trimmed) {
                ignore_next_added_line = true;
                current_line += 1;
                continue;
            }
            if should_ignore_secret_scan_line(content) {
                current_line += 1;
                continue;
            }
            if content.to_lowercase().contains("flow:secret:ignore-next") {
                ignore_next_added_line = true;
                current_line += 1;
                continue;
            }

            for (name, re) in patterns {
                if let Some(m) = re.find(content) {
                    let matched = m.as_str();
                    let matched_lower = matched.to_lowercase();

                    if matched_lower.contains("xxx")
                        || matched_lower.contains("your")
                        || matched_lower.contains("example")
                        || matched_lower.contains("placeholder")
                        || matched_lower.contains("replace")
                        || matched_lower.contains("insert")
                        || matched_lower.contains("todo")
                        || matched_lower.contains("fixme")
                        || matched == "sk-..."
                        || matched == "sk-xxxx"
                        || matched
                            .chars()
                            .all(|c| c == 'x' || c == 'X' || c == '.' || c == '-' || c == '_')
                    {
                        continue;
                    }

                    if *name == "Generic Secret Assignment"
                        && generic_secret_assignment_is_false_positive(content, matched)
                    {
                        continue;
                    }

                    let redacted = if matched.len() > 12 {
                        format!("{}...{}", &matched[..6], &matched[matched.len() - 4..])
                    } else {
                        matched.to_string()
                    };
                    findings.push((
                        current_file.clone(),
                        current_line,
                        name.to_string(),
                        redacted,
                    ));
                    break;
                }
            }
            current_line += 1;
        } else if !line.starts_with('-') && !line.starts_with('\\') {
            current_line += 1;
            ignore_next_added_line = false;
        }
    }

    findings
}
