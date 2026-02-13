use std::{
    env,
    fs::{self, OpenOptions},
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use crossterm::{event, terminal};

use crate::cli::DoctorOpts;
use crate::vcs;

/// Ensure the lin watcher daemon is available, prompting to install a bundled
/// copy if it is missing from PATH. Returns the resolved binary path.
pub fn ensure_lin_available_interactive() -> Result<PathBuf> {
    if let Ok(path) = which::which("lin") {
        println!("✅ lin watcher daemon found at {}", path.display());
        return Ok(path);
    }

    if let Some(bundled) = find_bundled_lin() {
        if prompt_install_lin(&bundled)? {
            let installed = install_lin(&bundled)?;
            println!("✅ Installed lin to {}", installed.display());
            return Ok(installed);
        }
    }

    bail!(
        "lin is not on PATH. Build/install from this repo (scripts/deploy.sh) so flow can delegate watchers to it."
    );
}

pub fn run(_opts: DoctorOpts) -> Result<()> {
    println!("Running flow doctor checks...\n");

    let zerobrew_available = ensure_zerobrew_available_interactive()?;

    ensure_flox_available(zerobrew_available)?;
    ensure_jj_available(zerobrew_available)?;
    let _ = ensure_lin_available_interactive();
    ensure_direnv_on_path(zerobrew_available)?;

    match detect_shell()? {
        Some(shell) => ensure_shell_hook(shell)?,
        None => println!(
            "⚠️  Unable to detect your shell from $SHELL. Add the direnv hook manually (see https://direnv.net)."
        ),
    }

    println!("\n✅ flow doctor is done. Re-run it any time after changing shells or machines.");
    Ok(())
}

fn ensure_flox_available(zerobrew_available: bool) -> Result<()> {
    if which::which("flox").is_ok() {
        println!("✅ flox found on PATH");
        return Ok(());
    }

    if maybe_install_with_zerobrew(zerobrew_available, "flox", "flox")? {
        if which::which("flox").is_ok() {
            println!("✅ flox installed via zerobrew");
            return Ok(());
        }
    }

    // Heuristic: flox-managed env leaves a .flox directory or ~/.flox directory.
    let home = home_dir();
    if home.join(".flox").exists() {
        println!(
            "✅ flox environment directory detected at {}",
            home.join(".flox").display()
        );
        return Ok(());
    }

    bail!(
        "flox is not installed. Install it from https://flox.dev/docs/install-flox/install/ and re-run `f doctor`."
    );
}

fn ensure_jj_available(zerobrew_available: bool) -> Result<()> {
    if which::which("jj").is_ok() {
        println!("✅ jj found on PATH");
        return Ok(());
    }

    if maybe_install_with_zerobrew(zerobrew_available, "jj", "jj")? {
        if which::which("jj").is_ok() {
            println!("✅ jj installed via zerobrew");
            return Ok(());
        }
    }

    vcs::ensure_jj_installed()?;
    println!("✅ jj found on PATH");
    Ok(())
}

fn ensure_direnv_on_path(zerobrew_available: bool) -> Result<()> {
    match which::which("direnv") {
        Ok(path) => {
            println!("✅ direnv found at {}", path.display());
            Ok(())
        }
        Err(_) => {
            if maybe_install_with_zerobrew(zerobrew_available, "direnv", "direnv")? {
                if let Ok(path) = which::which("direnv") {
                    println!("✅ direnv installed via zerobrew at {}", path.display());
                    return Ok(());
                }
            }
            bail!(
                "direnv is not on PATH. Install it from https://direnv.net/#installation and rerun `flow doctor`."
            )
        }
    }
}

fn find_bundled_lin() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))?;
    let candidate = exe_dir.join("lin");
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

fn prompt_install_lin(bundled: &Path) -> Result<bool> {
    println!(
        "lin was not found on PATH. A bundled copy was found at {}.",
        bundled.display()
    );
    print!(
        "Install lin to {}? [Y/n]: ",
        default_install_dir().display()
    );
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
    let normalized = input.trim().to_ascii_lowercase();
    Ok(normalized.is_empty() || normalized == "y" || normalized == "yes")
}

fn ensure_zerobrew_available_interactive() -> Result<bool> {
    if which::which("zb").is_ok() {
        println!("✅ zerobrew (zb) found on PATH");
        return Ok(true);
    }

    if !std::io::stdin().is_terminal() {
        println!("⚠️  zerobrew (zb) not found; skipping interactive install.");
        return Ok(false);
    }

    let install = prompt_yes("zerobrew (zb) not found. Install it now? [y/N]: ", false);

    if !install {
        return Ok(false);
    }

    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg("curl -sSL https://raw.githubusercontent.com/lucasgelfond/zerobrew/main/install.sh | bash")
        .status()
        .context("failed to run zerobrew install script")?;

    if status.success() {
        if which::which("zb").is_ok() {
            println!("✅ zerobrew installed");
            return Ok(true);
        }
        println!("⚠️  zerobrew installed but not on PATH yet; restart your shell.");
        return Ok(false);
    }

    println!("⚠️  zerobrew install failed");
    Ok(false)
}

fn maybe_install_with_zerobrew(
    zerobrew_available: bool,
    tool: &str,
    package: &str,
) -> Result<bool> {
    if !zerobrew_available {
        return Ok(false);
    }

    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }

    let prompt = format!("Install {} via zerobrew? [y/N]: ", tool);
    if !prompt_yes(&prompt, false) {
        return Ok(false);
    }

    let status = Command::new("zb")
        .arg("install")
        .arg(package)
        .status()
        .context("failed to run zb install")?;

    Ok(status.success())
}

fn prompt_yes(prompt: &str, default_yes: bool) -> bool {
    print!("{}", prompt);
    let _ = std::io::stdout().flush();
    if std::io::stdin().is_terminal() {
        if terminal::enable_raw_mode().is_ok() {
            let read = event::read();
            let _ = terminal::disable_raw_mode();
            if let Ok(event::Event::Key(key)) = read {
                let decision = match key.code {
                    event::KeyCode::Char('y') | event::KeyCode::Char('Y') => Some(true),
                    event::KeyCode::Char('n') | event::KeyCode::Char('N') => Some(false),
                    event::KeyCode::Enter => Some(default_yes),
                    event::KeyCode::Esc => Some(false),
                    _ => None,
                };
                if let Some(choice) = decision {
                    println!();
                    return choice;
                }
            }
        }
    }

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
    let normalized = input.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return default_yes;
    }
    normalized == "y" || normalized == "yes"
}

fn install_lin(bundled: &Path) -> Result<PathBuf> {
    let dest_dir = default_install_dir();
    std::fs::create_dir_all(&dest_dir).with_context(|| {
        format!(
            "failed to create lin install directory {}",
            dest_dir.display()
        )
    })?;

    let dest = dest_dir.join("lin");
    std::fs::copy(bundled, &dest).with_context(|| {
        format!(
            "failed to copy bundled lin from {} to {}",
            bundled.display(),
            dest.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest)
            .context("failed to stat installed lin")?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms).context("failed to mark lin executable")?;
    }

    Ok(dest)
}

fn default_install_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("bin"))
        .unwrap_or_else(|| PathBuf::from("."))
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
