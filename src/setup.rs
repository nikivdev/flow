use std::collections::BTreeMap;

use anyhow::Result;

use crate::{cli::SetupOpts, tasks::load_project_config};

pub fn run(opts: SetupOpts) -> Result<()> {
    let (config_path, cfg) = load_project_config(opts.config)?;

    if cfg.aliases.is_empty() {
        println!(
            "# No aliases defined in {}. Add an alias table like:",
            config_path.display()
        );
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
