use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::{
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
        bail!("flow config not found at {}", config_path.display());
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
