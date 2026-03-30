use std::io::IsTerminal;
use std::path::Path;

use crate::config;

#[derive(Debug, Clone)]
struct TaskFailureSettings {
    enabled: bool,
    tool: String,
    max_lines: usize,
    max_chars: usize,
    max_agents: usize,
}

impl Default for TaskFailureSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            tool: "hive".to_string(),
            max_lines: 80,
            max_chars: 8000,
            max_agents: 2,
        }
    }
}

fn load_settings() -> TaskFailureSettings {
    let mut settings = TaskFailureSettings::default();
    if let Some(ts_config) = config::load_ts_config() {
        if let Some(flow) = ts_config.flow {
            if let Some(task_failure) = flow.task_failure_agents {
                if let Some(enabled) = task_failure.enabled {
                    settings.enabled = enabled;
                }
                if let Some(tool) = task_failure.tool {
                    if !tool.trim().is_empty() {
                        settings.tool = tool;
                    }
                }
                if let Some(max_lines) = task_failure.max_lines {
                    settings.max_lines = max_lines.max(1);
                }
                if let Some(max_chars) = task_failure.max_chars {
                    settings.max_chars = max_chars.max(100);
                }
                if let Some(max_agents) = task_failure.max_agents {
                    settings.max_agents = max_agents.max(1);
                }
            }
        }
    }
    settings
}

fn task_failure_tool_message(tool: &str) -> Option<String> {
    let lowered = tool.trim().to_ascii_lowercase();
    if lowered.is_empty() || matches!(lowered.as_str(), "off" | "false" | "none" | "disabled") {
        return None;
    }
    if lowered == "hive" {
        return Some(
            "⚠ task-failure agents: Hive routing is retired and disabled. Use `flow.taskFailureHook` or run follow-up repair manually."
                .to_string(),
        );
    }
    Some(format!(
        "⚠ task-failure agents: unsupported tool '{}'",
        tool
    ))
}

pub fn maybe_run_task_failure_agents(
    task_name: &str,
    command: &str,
    workdir: &Path,
    output: &str,
    status: Option<i32>,
) {
    if let Ok(value) = std::env::var("FLOW_TASK_FAILURE_AGENTS") {
        let lowered = value.trim().to_lowercase();
        if lowered == "0" || lowered == "false" || lowered == "off" {
            return;
        }
    }
    if std::env::var("FLOW_DISABLE_TASK_FAILURE_AGENTS").is_ok() {
        return;
    }

    let settings = load_settings();
    if !settings.enabled {
        return;
    }
    let _ = (task_name, command, workdir, output, status);
    if let Some(message) = task_failure_tool_message(&settings.tool) {
        eprintln!("{}", message);
        return;
    }
    if !std::io::stdin().is_terminal() {
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::task_failure_tool_message;

    #[test]
    fn task_failure_tool_message_disables_empty_and_off_tools() {
        assert!(task_failure_tool_message("").is_none());
        assert!(task_failure_tool_message("off").is_none());
        assert!(task_failure_tool_message("disabled").is_none());
    }

    #[test]
    fn task_failure_tool_message_marks_hive_as_retired() {
        let message = task_failure_tool_message("hive").expect("hive should emit warning");
        assert!(message.contains("retired"));
        assert!(message.contains("taskFailureHook"));
    }
}
