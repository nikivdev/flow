use std::collections::HashSet;
use std::sync::OnceLock;

use regex::{Captures, Regex};
use serde_json::Value;

const REDACTED: &str = "[REDACTED]";

pub fn redact_text(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }

    let mut text = input.to_string();

    text = url_credentials_regex()
        .replace_all(&text, |caps: &Captures| {
            let prefix = caps.name("prefix").map(|m| m.as_str()).unwrap_or_default();
            format!("{prefix}{REDACTED}@")
        })
        .to_string();

    text = bearer_regex()
        .replace_all(&text, |caps: &Captures| {
            let prefix = caps.name("prefix").map(|m| m.as_str()).unwrap_or_default();
            format!("{prefix}{REDACTED}")
        })
        .to_string();

    text = quoted_assignment_regex()
        .replace_all(&text, |caps: &Captures| {
            let full = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
            let key = caps.name("key").map(|m| m.as_str()).unwrap_or_default();
            if !is_sensitive_key(key) {
                return full.to_string();
            }
            let value = caps.name("value").map(|m| m.as_str()).unwrap_or_default();
            if should_keep_assignment_value(value) {
                return full.to_string();
            }
            let prefix = caps.name("prefix").map(|m| m.as_str()).unwrap_or_default();
            let suffix = caps.name("suffix").map(|m| m.as_str()).unwrap_or_default();
            format!("{prefix}{REDACTED}{suffix}")
        })
        .to_string();

    text = unquoted_assignment_regex()
        .replace_all(&text, |caps: &Captures| {
            let full = caps.get(0).map(|m| m.as_str()).unwrap_or_default();
            let key = caps.name("key").map(|m| m.as_str()).unwrap_or_default();
            if !is_sensitive_key(key) {
                return full.to_string();
            }
            let value = caps.name("value").map(|m| m.as_str()).unwrap_or_default();
            if should_keep_assignment_value(value) {
                return full.to_string();
            }
            let prefix = caps.name("prefix").map(|m| m.as_str()).unwrap_or_default();
            format!("{prefix}{REDACTED}")
        })
        .to_string();

    text = known_token_regex().replace_all(&text, REDACTED).to_string();

    text = generic_token_regex()
        .replace_all(&text, |caps: &Captures| {
            let token = caps.name("token").map(|m| m.as_str()).unwrap_or_default();
            if looks_like_secretish_token(token) {
                REDACTED.to_string()
            } else {
                token.to_string()
            }
        })
        .to_string();

    text
}

pub fn redact_json_value(value: &mut Value) {
    match value {
        Value::String(s) => {
            *s = redact_text(s);
        }
        Value::Array(items) => {
            for item in items {
                redact_json_value(item);
            }
        }
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if is_sensitive_key(key) {
                    if let Value::String(_) = item {
                        *item = Value::String(REDACTED.to_string());
                        continue;
                    }
                }
                redact_json_value(item);
            }
        }
        _ => {}
    }
}

fn should_keep_assignment_value(value: &str) -> bool {
    let trimmed = value.trim().trim_matches('"').trim_matches('\'');
    if trimmed.is_empty() {
        return true;
    }
    if trimmed == REDACTED {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "true" | "false" | "null" | "none" | "undefined"
    ) {
        return true;
    }
    trimmed.starts_with('$') || trimmed.starts_with("${") || trimmed.starts_with("$(")
}

fn looks_like_secretish_token(token: &str) -> bool {
    if token.len() < 28 || token.len() > 256 {
        return false;
    }
    if token.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }

    let has_alpha = token.chars().any(|c| c.is_ascii_alphabetic());
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    if !has_alpha || !has_digit {
        return false;
    }

    let has_upper = token.chars().any(|c| c.is_ascii_uppercase());
    let has_symbol = token.contains('-') || token.contains('_');
    if !has_upper && !has_symbol {
        return false;
    }

    let mut unique = HashSet::new();
    for ch in token.chars() {
        unique.insert(ch);
    }
    if unique.len() < 8 {
        return false;
    }

    shannon_entropy(token) >= 3.6
}

fn is_sensitive_key(raw_key: &str) -> bool {
    if raw_key.is_empty() {
        return false;
    }
    let key = raw_key.to_ascii_lowercase();
    if key == "authorization" || key == "x-api-key" {
        return true;
    }
    let needles = [
        "token",
        "secret",
        "password",
        "passwd",
        "pwd",
        "api_key",
        "apikey",
        "private_key",
        "private-key",
        "client_secret",
        "client-secret",
        "bearer",
    ];
    needles.iter().any(|needle| key.contains(needle))
}

fn shannon_entropy(input: &str) -> f64 {
    if input.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for byte in input.bytes() {
        counts[usize::from(byte)] += 1;
    }
    let len = input.len() as f64;
    let mut entropy = 0.0f64;
    for count in counts {
        if count == 0 {
            continue;
        }
        let p = count as f64 / len;
        entropy -= p * p.log2();
    }
    entropy
}

fn url_credentials_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?P<prefix>https?://)(?P<creds>[^\s/@:]+:[^\s/@]+)@")
            .expect("valid url credentials regex")
    })
}

fn bearer_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?P<prefix>\bbearer\s+)(?P<token>[A-Za-z0-9._~+/=-]{12,})")
            .expect("valid bearer regex")
    })
}

fn quoted_assignment_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?P<prefix>["']?(?P<key>[A-Za-z_][A-Za-z0-9_-]{0,127})["']?\s*[:=]\s*["'])(?P<value>[^"'\\n]{4,})(?P<suffix>["'])"#,
        )
        .expect("valid quoted assignment regex")
    })
}

fn unquoted_assignment_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?P<prefix>["']?(?P<key>[A-Za-z_][A-Za-z0-9_-]{0,127})["']?\s*[:=]\s*)(?P<value>(?:bearer\s+)?[^\s,;'"\}\]]+)"#,
        )
        .expect("valid unquoted assignment regex")
    })
}

fn known_token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:ghp_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|glpat-[A-Za-z0-9_-]{20,}|xox[baprs]-[A-Za-z0-9-]{20,}|AKIA[0-9A-Z]{16}|sk-[A-Za-z0-9]{20,}|CFPAT-[A-Za-z0-9_-]{20,})\b",
        )
        .expect("valid known token regex")
    })
}

fn generic_token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?P<token>[A-Za-z0-9][A-Za-z0-9_-]{27,})\b")
            .expect("valid generic token regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_token() -> String {
        ["abcDEF0123", "456789TOKEN"].concat()
    }

    #[test]
    fn redacts_bearer_and_assignments() {
        let token = test_token();
        let input = format!("Authorization: Bearer {token}\nCLOUDFLARE_API_TOKEN={token}-foo");
        let redacted = redact_text(&input);
        assert!(redacted.contains("[REDACTED]"));
        assert!(redacted.contains("CLOUDFLARE_API_TOKEN="));
        assert!(!redacted.contains(&token));
    }

    #[test]
    fn redacts_url_credentials() {
        let input = "https://user:supersecret@example.com/path";
        let redacted = redact_text(input);
        assert_eq!(redacted, "https://[REDACTED]@example.com/path");
    }

    #[test]
    fn redacts_json_values_recursively() {
        let token = test_token();
        let mut value = json!({
            "headers": {"Authorization": format!("Bearer {token}")},
            "nested": [{"token": format!("{token}-foo")}]
        });
        redact_json_value(&mut value);
        let text = value.to_string();
        assert!(text.contains("[REDACTED]"));
        assert!(!text.contains(&token));
    }
}
