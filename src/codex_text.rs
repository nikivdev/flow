fn strip_tagged_block(text: &str, open_tag: &str, close_tag: &str) -> String {
    let mut result = text.to_string();
    while let Some(start) = result.find(open_tag) {
        if let Some(end) = result[start..].find(close_tag) {
            let end_pos = start + end + close_tag.len();
            result = format!("{}{}", &result[..start], &result[end_pos..]);
        } else {
            result = result[..start].to_string();
            break;
        }
    }
    result
}

fn strip_system_reminders(text: &str) -> String {
    strip_tagged_block(text, "<system-reminder>", "</system-reminder>")
        .trim()
        .to_string()
}

fn strip_agents_instruction_block(text: &str) -> String {
    let mut result = text.to_string();
    loop {
        let agents_start = result
            .find("# AGENTS.md instructions for ")
            .or_else(|| result.find("# agents.md instructions for "));
        let Some(start) = agents_start else {
            break;
        };

        if let Some(end) = result[start..].find("</INSTRUCTIONS>") {
            let end_pos = start + end + "</INSTRUCTIONS>".len();
            result = format!("{}{}", &result[..start], &result[end_pos..]);
        } else {
            result = result[..start].to_string();
            break;
        }
    }
    result
}

fn truncate_before_heading(text: &str, heading: &str) -> String {
    let mut offset = 0usize;
    for line in text.lines() {
        if line.trim_start().starts_with(heading) {
            return text[..offset].trim().to_string();
        }
        offset += line.len();
        if offset < text.len() {
            offset += 1;
        }
    }
    text.trim().to_string()
}

fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::new();
    let mut saw_blank = false;

    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            if saw_blank || out.is_empty() {
                continue;
            }
            saw_blank = true;
            out.push('\n');
            continue;
        }

        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(trimmed);
        out.push('\n');
        saw_blank = false;
    }

    out.trim().to_string()
}

pub(crate) fn sanitize_codex_memory_rollout_text(text: &str) -> Option<String> {
    let mut cleaned = strip_system_reminders(text);
    cleaned = strip_agents_instruction_block(&cleaned);
    cleaned = strip_tagged_block(&cleaned, "<skill>", "</skill>");
    cleaned = collapse_blank_lines(&cleaned);

    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

pub(crate) fn sanitize_codex_query_text(text: &str) -> Option<String> {
    let mut cleaned = sanitize_codex_memory_rollout_text(text)?;
    cleaned = strip_tagged_block(&cleaned, "<environment_context>", "</environment_context>");
    cleaned = strip_tagged_block(
        &cleaned,
        "<permissions instructions>",
        "</permissions instructions>",
    );
    cleaned = strip_tagged_block(&cleaned, "<collaboration_mode>", "</collaboration_mode>");
    cleaned = strip_tagged_block(
        &cleaned,
        "<subagent_notification>",
        "</subagent_notification>",
    );
    cleaned = truncate_before_heading(&cleaned, "Workflow context:");
    cleaned = truncate_before_heading(&cleaned, "Start by checking:");
    cleaned = truncate_before_heading(&cleaned, "Designer stack notes:");
    cleaned = collapse_blank_lines(&cleaned);

    let trimmed = cleaned.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<INSTRUCTIONS>")
        || trimmed.starts_with("# AGENTS.md instructions")
        || trimmed.starts_with("# agents.md instructions")
    {
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::{sanitize_codex_memory_rollout_text, sanitize_codex_query_text};

    #[test]
    fn rollout_sanitizer_drops_agents_and_skills_but_keeps_environment() {
        let text = concat!(
            "# AGENTS.md instructions for /tmp\n\n",
            "<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>\n",
            "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>\n",
            "<skill>\n<name>demo</name>\nbody\n</skill>\n",
            "<subagent_notification>{\"agent_id\":\"a\",\"status\":\"completed\"}</subagent_notification>\n"
        );

        let cleaned = sanitize_codex_memory_rollout_text(text).expect("cleaned");
        assert!(!cleaned.contains("AGENTS.md"));
        assert!(!cleaned.contains("<skill>"));
        assert!(cleaned.contains("<environment_context>"));
        assert!(cleaned.contains("<subagent_notification>"));
    }

    #[test]
    fn query_sanitizer_keeps_only_real_user_intent() {
        let text = concat!(
            "# AGENTS.md instructions for /tmp\n\n",
            "<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>\n",
            "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>\n",
            "write plan for rollout\n\n",
            "Workflow context:\n- Repo: ~/code/example\n"
        );

        assert_eq!(
            sanitize_codex_query_text(text).as_deref(),
            Some("write plan for rollout")
        );
    }

    #[test]
    fn query_sanitizer_drops_context_only_messages() {
        let text = concat!(
            "# AGENTS.md instructions for /tmp\n\n",
            "<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>\n",
            "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>\n"
        );

        assert_eq!(sanitize_codex_query_text(text), None);
    }
}
