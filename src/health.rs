use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::cli::HealthOpts;
use crate::doctor;

pub fn run(_opts: HealthOpts) -> Result<()> {
    println!("Running flow health checks...\n");

    ensure_fish_shell()?;
    ensure_fish_flow_init()?;

    doctor::run(crate::cli::DoctorOpts {})?;

    println!("\n✅ flow health checks passed.");
    Ok(())
}

fn ensure_fish_shell() -> Result<()> {
    let shell = env::var("SHELL").unwrap_or_default();
    if !shell.contains("fish") {
        let fish = which::which("fish")
            .context("fish is required; install it and ensure it is on PATH")?;
        bail!(
            "fish shell required. Run:\n  chsh -s {}",
            fish.display()
        );
    }
    Ok(())
}

fn ensure_fish_flow_init() -> Result<()> {
    let config_path = fish_config_path()?;
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let content = fs::read_to_string(&config_path).unwrap_or_default();
    if content.contains("# flow:start") {
        return Ok(());
    }

    let snippet = flow_fish_snippet();
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config_path)
        .with_context(|| format!("failed to open {}", config_path.display()))?;

    if !content.is_empty() && !content.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "{snippet}")?;

    println!(
        "✅ Added flow fish integration to {}. Restart your shell or run: source {}",
        config_path.display(),
        config_path.display()
    );
    Ok(())
}

fn fish_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("failed to resolve home directory")?;
    Ok(home.join("config").join("fish").join("config.fish"))
}

fn flow_fish_snippet() -> &'static str {
    r#"# flow:start
function f
    set -l bin ""
    if test -x ~/.local/bin/f
        set bin ~/.local/bin/f
    else if test -x ~/bin/f
        set bin ~/bin/f
    else
        set bin (command -v f)
    end

    if test -z "$argv[1]"
        $bin
    else
        $bin match $argv
    end
end
# flow:end
"#
}
