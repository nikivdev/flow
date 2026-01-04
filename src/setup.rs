use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{
    agents,
    cli::{SetupOpts, TaskRunOpts},
    start,
    tasks::{self, load_project_config},
};

pub fn run(opts: SetupOpts) -> Result<()> {
    let (project_root, config_path) = resolve_project_root(&opts.config)?;

    if !start::is_bootstrapped(&project_root) || !config_path.exists() {
        start::run_at(&project_root)?;
    }

    if !config_path.exists() {
        create_flow_toml_interactive(&project_root, &config_path)?;
    }

    let (config_path, cfg) = load_project_config(config_path)?;

    if tasks::find_task(&cfg, "setup").is_some() {
        return tasks::run(TaskRunOpts {
            config: config_path,
            delegate_to_hub: false,
            hub_host: std::net::IpAddr::from([127, 0, 0, 1]),
            hub_port: 9050,
            name: "setup".to_string(),
            args: Vec::new(),
        });
    }

    if cfg.aliases.is_empty() {
        println!(
            "# No setup task or aliases defined in {}.",
            config_path.display()
        );
        println!("# Add a setup task or an alias table like:");
        println!("#   [[alias]]");
        println!("#   fr = \"f run\"");
        return Ok(());
    }

    println!("# flow aliases from {}", config_path.display());
    println!(
        "# Apply them in your shell with: eval \"$(f setup --config {})\"",
        config_path.display()
    );

    for line in format_alias_lines(&cfg.aliases) {
        println!("{line}");
    }

    Ok(())
}

fn resolve_project_root(config_path: &PathBuf) -> Result<(PathBuf, PathBuf)> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let resolved = if config_path.is_absolute() {
        config_path.clone()
    } else {
        cwd.join(config_path)
    };
    let root = resolved
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or(cwd);
    Ok((root, resolved))
}

fn create_flow_toml_interactive(project_root: &Path, config_path: &Path) -> Result<()> {
    println!("No flow.toml found. Let's create one.");

    if !io::stdin().is_terminal() {
        let content = default_flow_template(project_root);
        write_flow_toml(config_path, &content)?;
        return Ok(());
    }

    let use_ai = prompt_yes_no("Generate setup/dev tasks with AI?", true)?;
    let mut content: Option<String> = None;

    if use_ai {
        let hint_input = prompt_optional("Any notes about how dev should run? (optional)")?;
        let hint = if hint_input.trim().is_empty() {
            None
        } else {
            Some(hint_input.as_str())
        };
        match generate_flow_toml_with_agent(project_root, hint) {
            Ok(text) => {
                if let Some(toml) = extract_flow_toml(&text) {
                    content = Some(toml);
                } else {
                    println!("Warning: AI output did not include flow.toml content.");
                }
            }
            Err(err) => {
                println!("Warning: AI generation failed: {}", err);
            }
        }
    }

    if content.is_none() {
        let defaults = suggested_commands(project_root);
        let setup_cmd = prompt_line("Setup command", defaults.setup.as_deref())?;
        let dev_cmd = prompt_line("Dev command", defaults.dev.as_deref())?;
        content = Some(render_flow_toml(
            &setup_cmd,
            &dev_cmd,
            defaults.deps,
        ));
    }

    let content = ensure_trailing_newline(content.unwrap_or_else(|| default_flow_template(project_root)));

    println!("\nProposed flow.toml:\n");
    println!("{}", content);

    if !prompt_yes_no("Write flow.toml?", true)? {
        bail!("aborted flow.toml creation");
    }

    write_flow_toml(config_path, &content)?;
    Ok(())
}

fn write_flow_toml(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content)
        .with_context(|| format!("failed to write {}", path.display()))?;
    println!("Created flow.toml");
    Ok(())
}

fn generate_flow_toml_with_agent(project_root: &Path, hint: Option<&str>) -> Result<String> {
    let mut prompt = String::new();
    prompt.push_str("Read the project and generate a minimal flow.toml with setup and dev tasks.\n");
    prompt.push_str("Requirements:\n");
    prompt.push_str("- Include only what is needed to make dev work reliably.\n");
    prompt.push_str("- The dev task must depend on setup (dependencies = [\"setup\"]).\n");
    prompt.push_str("- Add descriptions and shortcuts for setup (s) and dev (d).\n");
    prompt.push_str("- Use [deps] for required binaries.\n");
    prompt.push_str("- If a task prompts for input, set interactive = true.\n");
    prompt.push_str("- Output ONLY the flow.toml content, no commentary.\n\n");
    prompt.push_str("# flow.toml - Minimal spec\n\n");
    prompt.push_str("[deps]\n");
    prompt.push_str("mytool = \"rg\"\n");
    prompt.push_str("node = [\"node\", \"npm\"]\n\n");
    prompt.push_str("[[tasks]]\n");
    prompt.push_str("name = \"setup\"\n");
    prompt.push_str("command = \"cargo build --locked && npm ci\"\n");
    prompt.push_str("description = \"Install all tools & dependencies\"\n");
    prompt.push_str("activate_on_cd_to_root = true\n");
    prompt.push_str("shortcuts = [\"s\"]\n");
    prompt.push_str("dependencies = [\"rust\", \"node\"]\n\n");
    prompt.push_str("[[tasks]]\n");
    prompt.push_str("name = \"dev\"\n");
    prompt.push_str("command = \"cargo watch -x 'run --bin myapp'\"\n");
    prompt.push_str("description = \"Run development server with hot reload\"\n");
    prompt.push_str("dependencies = [\"setup\"]\n");
    prompt.push_str("shortcuts = [\"d\"]\n\n");

    let hints = project_hints(project_root);
    if !hints.is_empty() {
        prompt.push_str("Detected project hints:\n");
        for hint in hints {
            prompt.push_str("- ");
            prompt.push_str(&hint);
            prompt.push('\n');
        }
        prompt.push('\n');
    }

    if let Some(hint) = hint {
        if !hint.trim().is_empty() {
            prompt.push_str("User notes:\n");
            prompt.push_str(hint.trim());
            prompt.push('\n');
        }
    }

    agents::run_flow_agent_capture(&prompt)
}

fn extract_flow_toml(raw: &str) -> Option<String> {
    if let Some(block) = extract_fenced_block(raw, "toml") {
        return Some(block);
    }
    if let Some(block) = extract_fenced_block(raw, "") {
        return Some(block);
    }
    if raw.contains("[[tasks]]") {
        return Some(raw.trim().to_string());
    }
    None
}

fn extract_fenced_block(raw: &str, tag: &str) -> Option<String> {
    let fence = if tag.is_empty() {
        "```".to_string()
    } else {
        format!("```{tag}")
    };
    let start = raw.find(&fence)?;
    let after = &raw[start + fence.len()..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    let end = after.find("```")?;
    Some(after[..end].trim().to_string())
}

struct SuggestedCommands {
    setup: Option<String>,
    dev: Option<String>,
    deps: Vec<DepSpec>,
}

enum DepSpec {
    Single(&'static str, &'static str),
    Multiple(&'static str, &'static [&'static str]),
}

fn suggested_commands(project_root: &Path) -> SuggestedCommands {
    let cargo = project_root.join("Cargo.toml").exists();
    if cargo {
        return SuggestedCommands {
            setup: Some("cargo build --locked".to_string()),
            dev: Some("cargo run".to_string()),
            deps: vec![DepSpec::Single("cargo", "cargo")],
        };
    }

    let package_json = project_root.join("package.json").exists();
    if package_json {
        if project_root.join("pnpm-lock.yaml").exists() {
            return SuggestedCommands {
                setup: Some("pnpm install".to_string()),
                dev: Some("pnpm dev".to_string()),
                deps: vec![DepSpec::Single("pnpm", "pnpm")],
            };
        }
        if project_root.join("yarn.lock").exists() {
            return SuggestedCommands {
                setup: Some("yarn install".to_string()),
                dev: Some("yarn dev".to_string()),
                deps: vec![DepSpec::Single("yarn", "yarn")],
            };
        }
        if project_root.join("bun.lockb").exists() {
            return SuggestedCommands {
                setup: Some("bun install".to_string()),
                dev: Some("bun dev".to_string()),
                deps: vec![DepSpec::Single("bun", "bun")],
            };
        }
        if project_root.join("package-lock.json").exists() {
            return SuggestedCommands {
                setup: Some("npm ci".to_string()),
                dev: Some("npm run dev".to_string()),
                deps: vec![DepSpec::Multiple("node", &["node", "npm"])],
            };
        }
        return SuggestedCommands {
            setup: Some("npm install".to_string()),
            dev: Some("npm run dev".to_string()),
            deps: vec![DepSpec::Multiple("node", &["node", "npm"])],
        };
    }

    SuggestedCommands {
        setup: None,
        dev: None,
        deps: Vec::new(),
    }
}

fn default_flow_template(project_root: &Path) -> String {
    let defaults = suggested_commands(project_root);
    let setup_cmd = defaults.setup.unwrap_or_default();
    let dev_cmd = defaults.dev.unwrap_or_default();
    render_flow_toml(&setup_cmd, &dev_cmd, defaults.deps)
}

fn project_hints(project_root: &Path) -> Vec<String> {
    let mut hints = Vec::new();
    let candidates = [
        "Cargo.toml",
        "package.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "bun.lockb",
        "package-lock.json",
        "pyproject.toml",
        "requirements.txt",
        "Makefile",
        "justfile",
        "Dockerfile",
    ];
    for name in candidates {
        if project_root.join(name).exists() {
            hints.push(format!("{name}"));
        }
    }
    hints
}

fn render_flow_toml(setup_cmd: &str, dev_cmd: &str, deps: Vec<DepSpec>) -> String {
    let setup_cmd = setup_cmd.trim();
    let dev_cmd = dev_cmd.trim();
    let setup_cmd = if setup_cmd.is_empty() {
        "echo TODO: add setup command"
    } else {
        setup_cmd
    };
    let dev_cmd = if dev_cmd.is_empty() {
        "echo TODO: add dev command"
    } else {
        dev_cmd
    };

    let mut out = String::from("version = 1\n\n");
    out.push_str("[[tasks]]\n");
    out.push_str("name = \"setup\"\n");
    out.push_str(&format!("command = \"{}\"\n", toml_escape(setup_cmd)));
    out.push_str("description = \"Install tools and dependencies\"\n");
    out.push_str("shortcuts = [\"s\"]\n");
    if command_needs_interactive(setup_cmd) {
        out.push_str("interactive = true\n");
    }
    if !deps.is_empty() {
        out.push_str("dependencies = [");
        out.push_str(
            &deps
                .iter()
                .map(|d| format!("\"{}\"", dep_name(d)))
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push_str("]\n");
    }
    out.push('\n');
    out.push_str("[[tasks]]\n");
    out.push_str("name = \"dev\"\n");
    out.push_str(&format!("command = \"{}\"\n", toml_escape(dev_cmd)));
    out.push_str("description = \"Run development server\"\n");
    out.push_str("dependencies = [\"setup\"]\n");
    out.push_str("shortcuts = [\"d\"]\n");
    if command_needs_interactive(dev_cmd) {
        out.push_str("interactive = true\n");
    }

    if !deps.is_empty() {
        out.push('\n');
        out.push_str("[deps]\n");
        for dep in deps {
            match dep {
                DepSpec::Single(name, cmd) => {
                    out.push_str(&format!("{name} = \"{cmd}\"\n"));
                }
                DepSpec::Multiple(name, cmds) => {
                    let joined = cmds
                        .iter()
                        .map(|c| format!("\"{c}\""))
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push_str(&format!("{name} = [{joined}]\n"));
                }
            }
        }
    }

    out
}

fn command_needs_interactive(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("read -p")
        || lower.contains("read -s")
        || lower.contains("fzf")
        || lower.contains("password")
}

fn dep_name(dep: &DepSpec) -> &'static str {
    match dep {
        DepSpec::Single(name, _) => name,
        DepSpec::Multiple(name, _) => name,
    }
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn ensure_trailing_newline(mut content: String) -> String {
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content
}

fn prompt_yes_no(message: &str, default_yes: bool) -> Result<bool> {
    let prompt = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{message} {prompt}: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(default_yes);
    }
    Ok(answer == "y" || answer == "yes")
}

fn prompt_optional(message: &str) -> Result<String> {
    print!("{message}: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn prompt_line(message: &str, default: Option<&str>) -> Result<String> {
    if let Some(default) = default {
        print!("{message} [{default}]: ");
    } else {
        print!("{message}: ");
    }
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(default.unwrap_or("").to_string());
    }
    Ok(trimmed.to_string())
}

fn format_alias_lines(aliases: &std::collections::HashMap<String, String>) -> Vec<String> {
    let mut ordered = BTreeMap::new();
    for (name, target) in aliases {
        ordered.insert(name, target);
    }

    ordered
        .into_iter()
        .map(|(name, target)| format!("alias {name}='{}'", escape_single_quotes(target)))
        .collect()
}

fn escape_single_quotes(value: &str) -> String {
    value.replace('\'', "'\\''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn formats_alias_lines_in_order() {
        let mut aliases = HashMap::new();
        aliases.insert("fr".to_string(), "f run".to_string());
        aliases.insert("ft".to_string(), "f tasks".to_string());

        let lines = format_alias_lines(&aliases);
        assert_eq!(
            lines,
            vec![
                "alias fr='f run'".to_string(),
                "alias ft='f tasks'".to_string()
            ]
        );
    }

    #[test]
    fn escapes_single_quotes_in_commands() {
        let cmd = "echo 'hello'";
        assert_eq!(escape_single_quotes(cmd), "echo '\\''hello'\\''");
    }
}
