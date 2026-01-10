use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crossterm::event::{self, Event as CEvent, KeyCode};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use crate::cli::{ServicesAction, ServicesCommand, StripeModeArg, StripeServiceOpts};
use crate::{config, deploy, env};

pub fn run(cmd: ServicesCommand) -> Result<()> {
    match cmd.action {
        Some(ServicesAction::Stripe(opts)) => run_stripe(opts),
        Some(ServicesAction::List) | None => list_services(),
    }
}

fn list_services() -> Result<()> {
    println!("Available service setup flows:");
    println!("  stripe  - Guided Stripe env setup");
    Ok(())
}

pub fn maybe_run_stripe_setup(
    project_root: &Path,
    flow_cfg: &config::Config,
    env_name: &str,
) -> Result<()> {
    let stripe_keys = collect_stripe_keys(flow_cfg);
    if stripe_keys.is_empty() {
        return Ok(());
    }

    let required_keys = stripe_keys
        .iter()
        .filter(|key| stripe_key_spec(key).required)
        .cloned()
        .collect::<Vec<_>>();
    if required_keys.is_empty() {
        return Ok(());
    }

    let existing = fetch_project_env_vars_allow_missing(env_name, &required_keys)?;
    let missing_required = required_keys
        .iter()
        .filter(|key| {
            existing
                .get(*key)
                .map(|value| value.trim().is_empty())
                .unwrap_or(true)
        })
        .cloned()
        .collect::<Vec<_>>();

    if missing_required.is_empty() {
        println!("Stripe env vars already configured; skipping Stripe setup.");
        return Ok(());
    }

    println!("Stripe env vars missing: {}", missing_required.join(", "));
    if !prompt_yes_no("Run Stripe setup now?", true)? {
        return Ok(());
    }

    let mode_default = default_stripe_mode_for_env(env_name);
    let mode = prompt_stripe_mode(mode_default)?;

    run_stripe(StripeServiceOpts {
        path: Some(project_root.to_path_buf()),
        environment: Some(env_name.to_string()),
        mode,
        force: false,
        apply: false,
        no_apply: true,
    })
}

fn run_stripe(opts: StripeServiceOpts) -> Result<()> {
    let (project_root, flow_path, flow_cfg) = resolve_project_root(opts.path.as_ref())?;
    let _dir_guard = DirGuard::new(&project_root)?;

    let project_name = flow_cfg
        .project_name
        .clone()
        .or_else(|| {
            project_root
                .file_name()
                .and_then(|name| name.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "project".to_string());

    let env_name = opts.environment.clone().or_else(|| {
        flow_cfg
            .cloudflare
            .as_ref()
            .and_then(|cfg| cfg.environment.clone())
    });
    let env_name = env_name.unwrap_or_else(|| "production".to_string());

    println!("Stripe setup");
    println!("------------");
    println!("Project: {}", project_name);
    println!("Config:  {}", flow_path.display());
    println!("Env:     {}", env_name);
    println!(
        "Mode:    {}",
        match opts.mode {
            StripeModeArg::Test => "test",
            StripeModeArg::Live => "live",
        }
    );
    println!();

    let keys = collect_stripe_keys(&flow_cfg);
    let specs = keys
        .into_iter()
        .map(|key| stripe_key_spec(&key))
        .collect::<Vec<_>>();

    let key_names: Vec<String> = specs.iter().map(|spec| spec.key.clone()).collect();
    let existing = fetch_project_env_vars_allow_missing(&env_name, &key_names)?;

    let has_optional = specs.iter().any(|spec| !spec.required);
    let include_optional = if has_optional {
        prompt_yes_no("Set optional Stripe keys?", false)?
    } else {
        false
    };

    let mut missing_required = Vec::new();
    let mut updated = 0usize;

    for spec in specs {
        if !spec.required && !include_optional {
            continue;
        }

        if existing.contains_key(&spec.key) && !opts.force {
            println!("OK {} already set (use --force to update)", spec.key);
            continue;
        }

        println!();
        println!(
            "{}{}",
            spec.key,
            if spec.required { " (required)" } else { "" }
        );
        println!("  {}", spec.description);
        for line in spec.instructions(opts.mode) {
            println!("  - {}", line);
        }

        let value = if spec.secret {
            prompt_secret("Enter value (leave blank to skip)")?
        } else {
            prompt_line("Enter value (leave blank to skip)", None)?
        };
        let trimmed = value.trim();
        if trimmed.is_empty() {
            if spec.required {
                missing_required.push(spec.key.clone());
                println!("  WARN Skipped required key.");
            } else {
                println!("  Skipped.");
            }
            continue;
        }

        if let Some(prefix) = spec.expected_prefix(opts.mode) {
            if !trimmed.starts_with(prefix) {
                println!(
                    "  WARN Value does not look like {} (expected prefix: {}).",
                    spec.key, prefix
                );
            }
        }

        env::set_project_env_var(&spec.key, trimmed, &env_name, Some(spec.description))?;
        updated += 1;
    }

    println!();
    println!("Stripe setup complete. Updated {} key(s).", updated);
    if !missing_required.is_empty() {
        println!("Missing required keys:");
        for key in &missing_required {
            println!("  - {}", key);
        }
    }

    if should_apply_env(&opts) {
        apply_cloudflare_env(&project_root, &flow_cfg)?;
    } else {
        println!("Skipped applying envs to Cloudflare.");
    }

    Ok(())
}

fn apply_cloudflare_env(project_root: &Path, flow_cfg: &config::Config) -> Result<()> {
    let Some(cf) = flow_cfg.cloudflare.as_ref() else {
        println!("No [cloudflare] section found; skip apply.");
        return Ok(());
    };
    if !is_1focus_source(cf.env_source.as_deref()) {
        println!("cloudflare.env_source is not set to \"1focus\"; skip apply.");
        return Ok(());
    }
    deploy::apply_cloudflare_env(project_root, Some(flow_cfg))
}

fn should_apply_env(opts: &StripeServiceOpts) -> bool {
    if opts.apply {
        return true;
    }
    if opts.no_apply {
        return false;
    }
    prompt_yes_no("Apply envs to Cloudflare now?", true).unwrap_or(false)
}

fn is_1focus_source(source: Option<&str>) -> bool {
    matches!(
        source.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("1focus") | Some("1f") | Some("onefocus")
    )
}

fn fetch_project_env_vars_allow_missing(
    env_name: &str,
    keys: &[String],
) -> Result<HashMap<String, String>> {
    match env::fetch_project_env_vars(env_name, keys) {
        Ok(values) => Ok(values),
        Err(err) => {
            let msg = format!("{err:#}");
            if msg.contains("Project not found.") {
                println!("Project not found yet; it will be created on first set.");
                Ok(HashMap::new())
            } else {
                println!("Unable to read existing env vars: {err}");
                println!("Run `f env login` to authenticate with 1focus.");
                Err(err)
            }
        }
    }
}

fn default_stripe_mode_for_env(env_name: &str) -> StripeModeArg {
    if env_name.eq_ignore_ascii_case("production") {
        StripeModeArg::Live
    } else {
        StripeModeArg::Test
    }
}

fn prompt_stripe_mode(default: StripeModeArg) -> Result<StripeModeArg> {
    let default_label = match default {
        StripeModeArg::Test => "test",
        StripeModeArg::Live => "live",
    };
    let value = prompt_line("Stripe mode (test/live)", Some(default_label))?;
    match value.trim().to_ascii_lowercase().as_str() {
        "" => Ok(default),
        "test" | "t" => Ok(StripeModeArg::Test),
        "live" | "l" => Ok(StripeModeArg::Live),
        other => {
            println!("Unknown mode '{other}', using {default_label}.");
            Ok(default)
        }
    }
}

fn collect_stripe_keys(flow_cfg: &config::Config) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(cf) = flow_cfg.cloudflare.as_ref() {
        for key in cf.env_keys.iter().chain(cf.env_vars.iter()) {
            if is_stripe_key(key) && !keys.contains(key) {
                keys.push(key.clone());
            }
        }
    }
    if keys.is_empty() {
        keys = vec![
            "STRIPE_SECRET_KEY",
            "STRIPE_WEBHOOK_SECRET",
            "STRIPE_PRO_PRICE_ID",
            "STRIPE_REFILL_PRICE_ID",
            "VITE_STRIPE_PUBLISHABLE_KEY",
        ]
        .into_iter()
        .map(|key| key.to_string())
        .collect();
    }
    keys
}

fn is_stripe_key(key: &str) -> bool {
    key.starts_with("STRIPE_") || key.starts_with("VITE_STRIPE_")
}

struct StripeKeySpec {
    key: String,
    required: bool,
    secret: bool,
    description: &'static str,
    test_steps: &'static [&'static str],
    live_steps: &'static [&'static str],
    expected_test_prefix: Option<&'static str>,
    expected_live_prefix: Option<&'static str>,
}

impl StripeKeySpec {
    fn instructions(&self, mode: StripeModeArg) -> &'static [&'static str] {
        match mode {
            StripeModeArg::Test => self.test_steps,
            StripeModeArg::Live => self.live_steps,
        }
    }

    fn expected_prefix(&self, mode: StripeModeArg) -> Option<&'static str> {
        match mode {
            StripeModeArg::Test => self.expected_test_prefix,
            StripeModeArg::Live => self.expected_live_prefix,
        }
    }
}

fn stripe_key_spec(key: &str) -> StripeKeySpec {
    match key {
        "STRIPE_SECRET_KEY" => StripeKeySpec {
            key: key.to_string(),
            required: true,
            secret: true,
            description: "Server secret key for Stripe API access.",
            test_steps: &[
                "Stripe Dashboard (test mode) -> Developers -> API keys.",
                "Copy the Secret key (starts with sk_test_).",
            ],
            live_steps: &[
                "Stripe Dashboard (live mode) -> Developers -> API keys.",
                "Copy the Secret key (starts with sk_live_).",
            ],
            expected_test_prefix: Some("sk_test_"),
            expected_live_prefix: Some("sk_live_"),
        },
        "VITE_STRIPE_PUBLISHABLE_KEY" => StripeKeySpec {
            key: key.to_string(),
            required: true,
            secret: false,
            description: "Client publishable key for Stripe.js.",
            test_steps: &[
                "Stripe Dashboard (test mode) -> Developers -> API keys.",
                "Copy the Publishable key (starts with pk_test_).",
            ],
            live_steps: &[
                "Stripe Dashboard (live mode) -> Developers -> API keys.",
                "Copy the Publishable key (starts with pk_live_).",
            ],
            expected_test_prefix: Some("pk_test_"),
            expected_live_prefix: Some("pk_live_"),
        },
        "STRIPE_WEBHOOK_SECRET" => StripeKeySpec {
            key: key.to_string(),
            required: true,
            secret: true,
            description: "Webhook signing secret for Stripe events.",
            test_steps: &[
                "Local dev: run `stripe listen --print-secret` to get a whsec_... value.",
                "Or Stripe Dashboard (test mode) -> Developers -> Webhooks -> Add endpoint.",
            ],
            live_steps: &[
                "Stripe Dashboard (live mode) -> Developers -> Webhooks -> Add endpoint.",
                "Copy the Signing secret (starts with whsec_).",
            ],
            expected_test_prefix: Some("whsec_"),
            expected_live_prefix: Some("whsec_"),
        },
        "STRIPE_PRO_PRICE_ID" => StripeKeySpec {
            key: key.to_string(),
            required: true,
            secret: false,
            description: "Price ID for your main subscription plan.",
            test_steps: &[
                "Stripe Dashboard (test mode) -> Products -> select your plan.",
                "Copy the Price ID (starts with price_).",
            ],
            live_steps: &[
                "Stripe Dashboard (live mode) -> Products -> select your plan.",
                "Copy the Price ID (starts with price_).",
            ],
            expected_test_prefix: Some("price_"),
            expected_live_prefix: Some("price_"),
        },
        "STRIPE_REFILL_PRICE_ID" => StripeKeySpec {
            key: key.to_string(),
            required: false,
            secret: false,
            description: "Optional price ID for top-up/refill credits.",
            test_steps: &[
                "Stripe Dashboard (test mode) -> Products -> create a refill product.",
                "Copy the Price ID (starts with price_).",
            ],
            live_steps: &[
                "Stripe Dashboard (live mode) -> Products -> create a refill product.",
                "Copy the Price ID (starts with price_).",
            ],
            expected_test_prefix: Some("price_"),
            expected_live_prefix: Some("price_"),
        },
        _ => {
            let is_secret = key.contains("SECRET") || key.contains("WEBHOOK");
            StripeKeySpec {
                key: key.to_string(),
                required: false,
                secret: is_secret,
                description: "Stripe-related configuration value.",
                test_steps: &["Stripe Dashboard (test mode) -> copy the requested value."],
                live_steps: &["Stripe Dashboard (live mode) -> copy the requested value."],
                expected_test_prefix: None,
                expected_live_prefix: None,
            }
        }
    }
}

fn resolve_project_root(path: Option<&PathBuf>) -> Result<(PathBuf, PathBuf, config::Config)> {
    let start = match path {
        Some(path) => path.clone(),
        None => std::env::current_dir().context("failed to read current directory")?,
    };
    let flow_path = if start.is_file()
        && start.file_name().and_then(|name| name.to_str()) == Some("flow.toml")
    {
        start.clone()
    } else {
        find_flow_toml(&start)
            .ok_or_else(|| anyhow::anyhow!("flow.toml not found. Run from a Flow project."))?
    };
    let project_root = flow_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| start.clone());
    let flow_cfg = config::load(&flow_path)?;
    Ok((project_root, flow_path, flow_cfg))
}

fn find_flow_toml(start: &PathBuf) -> Option<PathBuf> {
    let mut current = start.clone();
    loop {
        let candidate = current.join("flow.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

struct DirGuard {
    previous: PathBuf,
}

impl DirGuard {
    fn new(path: &Path) -> Result<Self> {
        let previous = std::env::current_dir().context("failed to read current directory")?;
        std::env::set_current_dir(path)
            .with_context(|| format!("failed to switch to {}", path.display()))?;
        Ok(Self { previous })
    }
}

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
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

fn prompt_secret(message: &str) -> Result<String> {
    print!("{message}: ");
    io::stdout().flush()?;
    let input = rpassword::read_password().context("failed to read secret input")?;
    Ok(input.trim().to_string())
}

fn prompt_yes_no(message: &str, default_yes: bool) -> Result<bool> {
    let prompt = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{message} {prompt}: ");
    io::stdout().flush()?;
    if io::stdin().is_terminal() {
        return read_yes_no_key(default_yes);
    }
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(default_yes);
    }
    Ok(answer == "y" || answer == "yes")
}

fn read_yes_no_key(default_yes: bool) -> Result<bool> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut selection = default_yes;
    let mut echo_char: Option<char> = None;
    loop {
        if let CEvent::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    selection = true;
                    echo_char = Some('y');
                    break;
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    selection = false;
                    echo_char = Some('n');
                    break;
                }
                KeyCode::Enter => {
                    break;
                }
                KeyCode::Esc => {
                    selection = false;
                    break;
                }
                _ => {}
            }
        }
    }

    disable_raw_mode().context("failed to disable raw mode")?;
    if let Some(ch) = echo_char {
        println!("{ch}");
    } else {
        println!();
    }
    Ok(selection)
}
