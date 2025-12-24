//! Fix common TOML syntax errors in flow.toml files.
//!
//! Common issues that AI tools create:
//! - `\$` escape sequences (invalid in TOML basic strings)
//! - `\n` literal in basic strings instead of actual newlines
//! - Unclosed multi-line strings

use std::fs;

use anyhow::{Context, Result};
use regex::Regex;

use crate::cli::FixupOpts;

/// Result of a fixup operation.
#[derive(Debug)]
pub struct FixupResult {
    pub fixes_applied: Vec<FixupAction>,
    pub had_errors: bool,
}

#[derive(Debug)]
pub struct FixupAction {
    pub line: usize,
    pub description: String,
    pub before: String,
    pub after: String,
}

pub fn run(opts: FixupOpts) -> Result<()> {
    let config_path = if opts.config.is_absolute() {
        opts.config.clone()
    } else {
        std::env::current_dir()?.join(&opts.config)
    };

    if !config_path.exists() {
        anyhow::bail!("flow.toml not found at {}", config_path.display());
    }

    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;

    let result = fix_toml_content(&content);

    if result.fixes_applied.is_empty() {
        println!("✓ No issues found in {}", config_path.display());
        return Ok(());
    }

    println!(
        "Found {} issue(s) in {}:\n",
        result.fixes_applied.len(),
        config_path.display()
    );

    for fix in &result.fixes_applied {
        println!("  Line {}: {}", fix.line, fix.description);
        println!("    - {}", truncate_for_display(&fix.before, 60));
        println!("    + {}", truncate_for_display(&fix.after, 60));
        println!();
    }

    if opts.dry_run {
        println!("Dry run - no changes written.");
        return Ok(());
    }

    // Apply fixes
    let fixed_content = apply_fixes(&content, &result.fixes_applied);

    // Validate the fixed content parses
    if let Err(e) = toml::from_str::<toml::Value>(&fixed_content) {
        println!("⚠ Warning: Fixed content still has TOML errors: {}", e);
        println!("Writing anyway - manual review recommended.");
    }

    fs::write(&config_path, &fixed_content)
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    println!(
        "✓ Fixed {} issue(s) in {}",
        result.fixes_applied.len(),
        config_path.display()
    );

    Ok(())
}

/// Fix common TOML issues in the content.
pub fn fix_toml_content(content: &str) -> FixupResult {
    let mut fixes = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    // Track if we're inside a multi-line basic string (""")
    let mut in_multiline_basic = false;
    let mut _multiline_start_line = 0;

    for (line_idx, line) in lines.iter().enumerate() {
        let line_num = line_idx + 1;

        // Count triple quotes to track multi-line string state
        let triple_quote_count = line.matches(r#"""""#).count();

        if !in_multiline_basic {
            // Check for start of multi-line basic string
            if triple_quote_count == 1 {
                in_multiline_basic = true;
                _multiline_start_line = line_num;
            } else if triple_quote_count == 2 {
                // Single-line multi-line string (opens and closes on same line)
                // Check for issues in this line
                if let Some(fix) = check_invalid_escapes(line, line_num) {
                    fixes.push(fix);
                }
            }
        } else {
            // Inside multi-line basic string
            if triple_quote_count >= 1 {
                // End of multi-line string
                in_multiline_basic = false;
            }

            // Check for invalid escape sequences inside multi-line basic strings
            if let Some(fix) = check_invalid_escapes(line, line_num) {
                fixes.push(fix);
            }
        }
    }

    FixupResult {
        fixes_applied: fixes,
        had_errors: false,
    }
}

/// Apply fixes to TOML content and return the updated string.
pub fn apply_fixes_to_content(content: &str, fixes: &[FixupAction]) -> String {
    apply_fixes(content, fixes)
}

/// Check a line for invalid escape sequences in TOML basic strings.
fn check_invalid_escapes(line: &str, line_num: usize) -> Option<FixupAction> {
    // Invalid escapes in TOML basic strings: \$ \: \@ \! etc.
    // Valid escapes: \\ \n \t \r \" \b \f \uXXXX \UXXXXXXXX and \ followed by newline
    // We need to find backslash followed by characters that are NOT valid escape chars
    let invalid_escape_re = Regex::new(r#"\\([^\\nrtbf"uU\s])"#).unwrap();

    if let Some(capture) = invalid_escape_re.find(line) {
        let escaped_char = &line[capture.start() + 1..capture.end()];
        let fixed_line = invalid_escape_re
            .replace_all(line, |caps: &regex::Captures| {
                // Just remove the backslash, keep the character
                caps[1].to_string()
            })
            .to_string();

        return Some(FixupAction {
            line: line_num,
            description: format!("Invalid escape sequence '\\{}'", escaped_char),
            before: line.to_string(),
            after: fixed_line,
        });
    }

    None
}

/// Apply fixes to content, returning the fixed string.
fn apply_fixes(content: &str, fixes: &[FixupAction]) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut result_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();

    for fix in fixes {
        if fix.line > 0 && fix.line <= result_lines.len() {
            result_lines[fix.line - 1] = fix.after.clone();
        }
    }

    // Preserve original line endings
    let has_trailing_newline = content.ends_with('\n');
    let mut result = result_lines.join("\n");
    if has_trailing_newline {
        result.push('\n');
    }

    result
}

fn truncate_for_display(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixes_escaped_dollar() {
        let content = r##"
[[tasks]]
name = "test"
command = """
echo "Price: \$8"
"""
"##;
        let result = fix_toml_content(content);
        assert_eq!(result.fixes_applied.len(), 1);
        assert!(result.fixes_applied[0].description.contains(r"\$"));
    }

    #[test]
    fn preserves_valid_escapes() {
        let content = r##"
[[tasks]]
name = "test"
command = """
echo "Line1"
echo "Tab here"
"""
"##;
        let result = fix_toml_content(content);
        assert!(result.fixes_applied.is_empty());
    }

    #[test]
    fn no_fixes_needed() {
        let content = r#"
[[tasks]]
name = "test"
command = "echo hello"
"#;
        let result = fix_toml_content(content);
        assert!(result.fixes_applied.is_empty());
    }
}
