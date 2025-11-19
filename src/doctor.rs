use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::cli::DoctorOpts;

pub fn run(_opts: DoctorOpts) -> Result<()> {
    println!("Running flow doctor checks...\n");

    ensure_direnv_on_path()?;

    match detect_shell()? {
        Some(shell) => ensure_shell_hook(shell)?,
        None => println!(
            "⚠️  Unable to detect your shell from $SHELL. Add the direnv hook manually (see https://direnv.net)."
        ),
    }

    println!("\n✅ flow doctor is done. Re-run it any time after changing shells or machines.");
    Ok(())
}

fn ensure_direnv_on_path() -> Result<()> {
    match which::which("direnv") {
        Ok(path) => {
            println!("✅ direnv found at {}", path.display());
            Ok(())
        }
        Err(_) => bail!(
            "direnv is not on PATH. Install it from https://direnv.net/#installation and rerun `flow doctor`."
        ),
    }
}

fn detect_shell() -> Result<Option<ShellKind>> {
    if let Ok(shell_path) = env::var("SHELL") {
        if let Some(kind) = ShellKind::from_path(shell_path) {
            println!("✅ Detected shell: {}", kind.display());
            return Ok(Some(kind));
        }
    }
    Ok(None)
}

fn ensure_shell_hook(shell: ShellKind) -> Result<()> {
    let config_path = shell.config_path();
    let indicator = shell.hook_indicator();
    let snippet = shell.hook_snippet();

    let existing = fs::read_to_string(&config_path).unwrap_or_default();
    if existing.contains(indicator) {
        println!(
            "✅ {} already sources direnv ({}).",
            shell.display(),
            config_path.display()
        );
        return Ok(());
    }

    println!(
        "ℹ️  Adding direnv hook to {} ({}).",
        shell.display(),
        config_path.display()
    );

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.to_string_lossy()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config_path)
        .with_context(|| format!("failed to open {}", config_path.display()))?;

    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }

    writeln!(file, "\n# Added by flow doctor")?;
    writeln!(file, "{snippet}")?;

    println!(
        "✅ Added direnv hook for {}. Restart your shell or source {}.",
        shell.display(),
        config_path.display()
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellKind {
    Bash,
    Zsh,
    Fish,
}

impl ShellKind {
    fn from_path<P: AsRef<Path>>(path: P) -> Option<Self> {
        let name = path
            .as_ref()
            .file_name()
            .map(|os| os.to_string_lossy().to_ascii_lowercase())?;
        match name.as_str() {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }

    fn display(&self) -> &'static str {
        match self {
            ShellKind::Bash => "bash",
            ShellKind::Zsh => "zsh",
            ShellKind::Fish => "fish",
        }
    }

    fn config_path(&self) -> PathBuf {
        let home = home_dir();
        self.config_path_with_base(&home)
    }

    fn config_path_with_base(&self, home: &Path) -> PathBuf {
        match self {
            ShellKind::Bash => home.join(".bashrc"),
            ShellKind::Zsh => home.join(".zshrc"),
            ShellKind::Fish => home.join(".config/fish/config.fish"),
        }
    }

    fn hook_indicator(&self) -> &'static str {
        match self {
            ShellKind::Bash => "direnv hook bash",
            ShellKind::Zsh => "direnv hook zsh",
            ShellKind::Fish => "direnv hook fish",
        }
    }

    fn hook_snippet(&self) -> &'static str {
        match self {
            ShellKind::Bash => {
                r#"if command -v direnv >/dev/null 2>&1; then
    eval "$(direnv hook bash)"
fi"#
            }
            ShellKind::Zsh => {
                r#"if command -v direnv >/dev/null 2>&1; then
    eval "$(direnv hook zsh)"
fi"#
            }
            ShellKind::Fish => {
                r#"if type -q direnv
    direnv hook fish | source
end"#
            }
        }
    }
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_detection_from_path() {
        assert_eq!(ShellKind::from_path("/bin/bash"), Some(ShellKind::Bash));
        assert_eq!(ShellKind::from_path("zsh"), Some(ShellKind::Zsh));
        assert_eq!(
            ShellKind::from_path("/usr/local/bin/fish"),
            Some(ShellKind::Fish)
        );
        assert_eq!(ShellKind::from_path("/bin/sh"), None);
    }

    #[test]
    fn config_paths_follow_home_env() {
        let base = Path::new("/tmp/drflow");
        assert_eq!(
            ShellKind::Zsh.config_path_with_base(base),
            PathBuf::from("/tmp/drflow/.zshrc")
        );
        assert_eq!(
            ShellKind::Bash.config_path_with_base(base),
            PathBuf::from("/tmp/drflow/.bashrc")
        );
        assert_eq!(
            ShellKind::Fish.config_path_with_base(base),
            PathBuf::from("/tmp/drflow/.config/fish/config.fish")
        );
    }
}
