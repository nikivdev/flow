use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crossterm::event::{self, Event as CEvent, KeyCode};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use serde::Deserialize;

use crate::{
    agents,
    cli::{SetupOpts, SetupTarget, TaskRunOpts},
    config,
    deploy,
    start,
    tasks::{self, load_project_config},
};

pub fn run(opts: SetupOpts) -> Result<()> {
    let (project_root, config_path) = resolve_project_root(&opts.config)?;
    let mut created_flow_toml = false;

    if !start::is_bootstrapped(&project_root) || !config_path.exists() {
        start::run_at(&project_root)?;
    }

    match opts.target {
        Some(SetupTarget::Deploy) => {
            return setup_deploy(&project_root, &config_path);
        }
        Some(SetupTarget::Release) => {
            return setup_release(&project_root, &config_path);
        }
        None => {}
    }

    if !config_path.exists() {
        create_flow_toml_interactive(&project_root, &config_path)?;
        created_flow_toml = true;
    }

    let (config_path, cfg) = load_project_config(config_path)?;

    if tasks::find_task(&cfg, "setup").is_some() {
        if created_flow_toml {
            if io::stdin().is_terminal() {
                if !prompt_yes_no("Run setup task now?", false)? {
                    println!("Skipping setup. Review flow.toml, then run `f setup` or `f run setup`.");
                    return Ok(());
                }
            } else {
                println!("Skipping setup for newly created flow.toml. Run `f setup` or `f run setup` when ready.");
                return Ok(());
            }
        }
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

fn setup_deploy(project_root: &Path, config_path: &Path) -> Result<()> {
    let server_reason = detect_server_project(project_root);
    let auto_mode = server_reason.is_some();

    if !config_path.exists() {
        if auto_mode {
            create_flow_toml_auto(project_root, config_path)?;
        } else {
            create_flow_toml_interactive(project_root, config_path)?;
        }
    }

    let mut flow_content = fs::read_to_string(config_path).unwrap_or_default();
    if has_host_section(&flow_content) {
        if auto_mode {
            repair_existing_host_config(project_root, config_path, &flow_content)?;
        } else {
            println!("flow.toml already includes [host] configuration.");
        }
        return Ok(());
    }

    let is_tty = io::stdin().is_terminal();
    let mut defaults = deploy_defaults(project_root);

    if let Some(reason) = server_reason.as_deref() {
        println!("Detected server project: {reason}");
        if !auto_mode && is_tty && !prompt_yes_no("Configure Linux host deployment now?", true)? {
            println!("Skipped host setup. Run `f setup deploy` later to configure.");
            return Ok(());
        }

        let _ = deploy::ensure_deploy_helper();

        let template = load_server_setup_template();
        if let Some(template) = template.as_ref() {
            println!("Using server setup template from {}.", template.source);
        }
        apply_server_template(&mut defaults, template.as_ref(), project_root);

        if !auto_mode && is_tty && prompt_yes_no("Use AI to draft host config?", true)? {
            println!("Generating host config with AI...");
            io::stdout().flush()?;
            let result = generate_host_config_with_agent(project_root, None);
            match result {
                Ok(text) => {
                    if let Some(host_cfg) = extract_host_config(&text) {
                        if let Some(reason) = host_config_mismatch_reason(project_root, &host_cfg) {
                            println!("Warning: {}", reason);
                            println!("Using detected defaults instead.");
                        } else {
                            apply_host_overrides(&mut defaults, &host_cfg);
                        }
                    } else {
                        println!("Warning: AI output did not include [host] config.");
                    }
                }
                Err(err) => {
                    println!("Warning: AI generation failed: {}", err);
                }
            }
        }
    }

    let (dest, run, service, setup_script, env_file, domain, ssl, port) =
        if server_reason.is_some() {
            (
                defaults.dest.clone(),
                defaults.run.clone(),
                Some(defaults.service.clone()),
                normalize_optional(defaults.setup_path.clone()),
                defaults.env_file.clone(),
                defaults.domain.clone(),
                defaults.ssl && defaults.domain.is_some(),
                if defaults.domain.is_some() {
                    defaults.port
                } else {
                    None
                },
            )
        } else {
            let dest = if is_tty {
                prompt_line("Remote deploy path", Some(&defaults.dest))?
            } else {
                defaults.dest.clone()
            };

            let run = if is_tty {
                let value = prompt_line("Run command", defaults.run.as_deref())?;
                normalize_optional(value)
            } else {
                defaults.run.clone()
            };

            if run.is_none() {
                println!("Warning: no run command set; deploy will not create a systemd service.");
            }

            let service = if is_tty {
                let value = prompt_line("Systemd service name", Some(&defaults.service))?;
                normalize_optional(value)
            } else {
                Some(defaults.service.clone())
            };

            let setup_script = if is_tty {
                let value = prompt_line(
                    "Setup script path (relative to repo)",
                    Some(&defaults.setup_path),
                )?;
                normalize_optional(value)
            } else {
                Some(defaults.setup_path.clone())
            };

            let env_file = if is_tty {
                prompt_line_optional(
                    "Env file to upload (copied to remote as .env)",
                    defaults.env_file.as_deref(),
                )?
            } else {
                defaults.env_file.clone()
            };

            let domain = if is_tty {
                prompt_line_optional("Domain (blank to skip)", defaults.domain.as_deref())?
            } else {
                defaults.domain.clone()
            };

            let ssl = if is_tty && domain.is_some() {
                prompt_yes_no("Enable SSL via Let's Encrypt?", defaults.ssl)?
            } else {
                defaults.ssl && domain.is_some()
            };

            let port = if domain.is_some() {
                if is_tty {
                    prompt_u16_optional("Service port for nginx", defaults.port)?
                } else {
                    defaults.port
                }
            } else {
                None
            };

            (dest, run, service, setup_script, env_file, domain, ssl, port)
        };

    if server_reason.is_some() && run.is_none() {
        println!("Warning: no run command set; deploy will not create a systemd service.");
    }

    if let Some(script_path) = setup_script.as_ref() {
        if let Some(content) = defaults.setup_script_content.as_deref() {
            ensure_setup_script(project_root, script_path, content, false)?;
        }
    }

    if let Some(env_path) = env_file.as_ref() {
        ensure_env_file(
            project_root,
            env_path,
            defaults.env_example.as_ref(),
            !auto_mode && is_tty,
            auto_mode,
        )?;
    }

    if auto_mode {
        maybe_configure_deploy_host(true)?;
    } else if is_tty {
        maybe_configure_deploy_host(false)?;
    }

    let host_cfg = HostSetupConfig {
        dest,
        setup: setup_script,
        run,
        port,
        service,
        env_file,
        domain,
        ssl,
    };

    let host_section = render_host_section(&host_cfg);
    flow_content = append_section(&flow_content, &host_section);
    fs::write(config_path, flow_content)
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    println!("Added [host] config to flow.toml.");
    println!("Next: run `f deploy` to deploy.");
    Ok(())
}

fn setup_release(project_root: &Path, config_path: &Path) -> Result<()> {
    if !config_path.exists() {
        create_flow_toml_interactive(project_root, config_path)?;
    }

    let mut flow_content = fs::read_to_string(config_path).unwrap_or_default();
    if has_host_section(&flow_content) {
        println!("flow.toml already includes [host] configuration.");
        return Ok(());
    }

    let Some(reason) = detect_server_project(project_root) else {
        println!("No server project detected. Add [host] manually or run `f setup deploy`.");
        return Ok(());
    };
    println!("Detected server project: {reason}");

    if io::stdin().is_terminal() && !prompt_yes_no("Configure Linux host deployment now?", true)? {
        println!("Skipped host setup. Run `f setup deploy` or edit flow.toml later.");
        return Ok(());
    }

    let template = load_server_setup_template();
    if let Some(template) = template.as_ref() {
        println!("Using server setup template from {}.", template.source);
    }

    let mut defaults = deploy_defaults(project_root);
    apply_server_template(&mut defaults, template.as_ref(), project_root);

    if defaults.run.is_none() {
        println!("Warning: no run command set; deploy will not create a systemd service.");
    }

    if let Some(content) = defaults.setup_script_content.as_deref() {
        if !defaults.setup_path.trim().is_empty() {
            ensure_setup_script(project_root, &defaults.setup_path, content, false)?;
        }
    }

    if let Some(env_path) = defaults.env_file.as_ref() {
        ensure_env_file(project_root, env_path, defaults.env_example.as_ref(), false, false)?;
    }

    if io::stdin().is_terminal() {
        maybe_configure_deploy_host(false)?;
    }

    let host_cfg = HostSetupConfig {
        dest: defaults.dest,
        setup: normalize_optional(defaults.setup_path),
        run: defaults.run,
        port: defaults.port,
        service: Some(defaults.service),
        env_file: defaults.env_file,
        domain: defaults.domain,
        ssl: defaults.ssl,
    };

    let host_section = render_host_section(&host_cfg);
    flow_content = append_section(&flow_content, &host_section);
    fs::write(config_path, flow_content)
        .with_context(|| format!("failed to write {}", config_path.display()))?;

    println!("Added [host] config to flow.toml.");
    println!("Next: run `f deploy` to deploy.");
    Ok(())
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
    let mut streamed_ai_output = false;
    let mut used_ai_content = false;

    if use_ai {
        let hint_input = prompt_optional("Any notes about how dev should run? (optional)")?;
        let hint = if hint_input.trim().is_empty() {
            None
        } else {
            Some(hint_input.as_str())
        };
        println!("Generating flow.toml with AI...");
        io::stdout().flush()?;
        let use_streaming = io::stdin().is_terminal();
        let result = if use_streaming {
            generate_flow_toml_with_agent_streaming(project_root, hint)
        } else {
            generate_flow_toml_with_agent(project_root, hint)
        };
        match result {
            Ok(text) => {
                if use_streaming {
                    streamed_ai_output = true;
                }
                if let Some(toml) = extract_flow_toml(&text) {
                    if let Some(reason) = ai_flow_toml_mismatch_reason(project_root, &toml) {
                        println!("Warning: {}", reason);
                        println!("Using detected defaults instead.");
                    } else {
                        content = Some(toml);
                        used_ai_content = true;
                    }
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
        let setup_cmd = defaults.setup.unwrap_or_default();
        let dev_cmd = defaults.dev.unwrap_or_default();
        content = Some(render_flow_toml(&setup_cmd, &dev_cmd, defaults.deps));
        println!("Using detected defaults. Edit flow.toml if needed.");
    }

    let content = ensure_trailing_newline(content.unwrap_or_else(|| default_flow_template(project_root)));

    if !used_ai_content || !streamed_ai_output {
        println!("\nProposed flow.toml:\n");
        println!("{}", content);
    }
    write_flow_toml(config_path, &content)?;
    Ok(())
}

fn create_flow_toml_auto(project_root: &Path, config_path: &Path) -> Result<()> {
    println!("No flow.toml found. Creating defaults.");
    let content = ensure_trailing_newline(default_flow_template(project_root));
    write_flow_toml(config_path, &content)
}

fn repair_existing_host_config(
    project_root: &Path,
    config_path: &Path,
    flow_content: &str,
) -> Result<()> {
    let Some(reason) = detect_server_project(project_root) else {
        println!("flow.toml already includes [host] configuration.");
        return Ok(());
    };
    println!("Detected server project: {reason}");

    let cfg = config::load(config_path)?;
    let Some(mut host_cfg) = cfg.host else {
        println!("flow.toml already includes [host] configuration.");
        return Ok(());
    };

    let mut defaults = deploy_defaults(project_root);
    let template = load_server_setup_template();
    apply_server_template(&mut defaults, template.as_ref(), project_root);

    let mut changed = false;
    let mut force_setup_script = false;

    if host_cfg.dest.is_none() {
        host_cfg.dest = Some(defaults.dest.clone());
        changed = true;
    }

    if host_cfg.run.is_none() {
        if let Some(run) = defaults.run.clone() {
            host_cfg.run = Some(run);
            changed = true;
        }
    } else if let Some(run) = host_cfg.run.as_deref() {
        if let Some(default_run) = defaults.run.clone() {
            if let Some(reason) = command_mismatch_reason(project_root, run) {
                println!("Warning: replacing run command: {reason}");
                host_cfg.run = Some(default_run);
                changed = true;
            }
        }
    }

    if host_cfg.service.is_none() {
        host_cfg.service = Some(defaults.service.clone());
        changed = true;
    }

    if host_cfg.setup.is_none() {
        if !defaults.setup_path.trim().is_empty() {
            host_cfg.setup = Some(defaults.setup_path.clone());
            changed = true;
        }
    } else if let Some(setup) = host_cfg.setup.as_deref() {
        if let Some(reason) = setup_script_mismatch_reason(project_root, setup) {
            println!("Warning: replacing setup script: {reason}");
            if !defaults.setup_path.trim().is_empty() {
                host_cfg.setup = Some(defaults.setup_path.clone());
                changed = true;
                force_setup_script = true;
            }
        }
    }

    if host_cfg.env_file.is_none() {
        if let Some(env_file) = defaults.env_file.clone() {
            host_cfg.env_file = Some(env_file);
            changed = true;
        }
    }

    if let Some(setup_path) = host_cfg.setup.as_deref() {
        if let Some(content) = defaults.setup_script_content.as_deref() {
            ensure_setup_script(project_root, setup_path, content, force_setup_script)?;
        }
    }

    if let Some(env_path) = host_cfg.env_file.as_deref() {
        ensure_env_file(
            project_root,
            env_path,
            defaults.env_example.as_ref(),
            false,
            true,
        )?;
    }

    maybe_configure_deploy_host(true)?;

    if host_cfg.run.is_none() {
        println!("Warning: no run command set; deploy will not create a systemd service.");
    }

    if changed {
        let host_section = render_host_section(&HostSetupConfig {
            dest: host_cfg.dest.unwrap_or_else(|| defaults.dest.clone()),
            setup: host_cfg.setup,
            run: host_cfg.run,
            port: host_cfg.port,
            service: host_cfg.service,
            env_file: host_cfg.env_file,
            domain: host_cfg.domain,
            ssl: host_cfg.ssl,
        });
        let updated = replace_host_section(flow_content, &host_section);
        fs::write(config_path, updated)
            .with_context(|| format!("failed to write {}", config_path.display()))?;
        println!("Updated [host] config in flow.toml.");
    } else {
        println!("Host config looks good.");
    }

    Ok(())
}

struct DeployDefaults {
    dest: String,
    run: Option<String>,
    service: String,
    setup_path: String,
    setup_script_content: Option<String>,
    env_example: Option<PathBuf>,
    env_file: Option<String>,
    port: Option<u16>,
    domain: Option<String>,
    ssl: bool,
}

struct HostSetupConfig {
    dest: String,
    setup: Option<String>,
    run: Option<String>,
    port: Option<u16>,
    service: Option<String>,
    env_file: Option<String>,
    domain: Option<String>,
    ssl: bool,
}

struct ServerSetupTemplate {
    host: deploy::HostConfig,
    source: String,
}

fn deploy_defaults(project_root: &Path) -> DeployDefaults {
    let project_name = guess_project_name(project_root);
    let dest = format!("/opt/{}", project_name);
    let run = default_run_command(project_root, &project_name);
    let service = project_name.clone();
    let setup_path = "deploy/setup.sh".to_string();
    let setup_script_content = Some(default_setup_script(project_root));
    let env_example = find_env_example(project_root, &project_name);
    let env_file = env_example
        .as_ref()
        .and_then(|path| strip_example_suffix(project_root, path));
    let port = Some(8080);
    let domain = None;
    let ssl = false;

    DeployDefaults {
        dest,
        run,
        service,
        setup_path,
        setup_script_content,
        env_example,
        env_file,
        port,
        domain,
        ssl,
    }
}

fn load_server_setup_template() -> Option<ServerSetupTemplate> {
    let mut host_config: Option<deploy::HostConfig> = None;
    let mut source: Option<String> = None;

    let global_path = config::default_config_path();
    if global_path.exists() {
        if let Ok(cfg) = config::load(&global_path) {
            if let Some(setup) = cfg.setup {
                if let Some(server) = setup.server {
                    if let Some(template_path) = server.template {
                        let path = config::expand_path(&template_path);
                        if path.exists() {
                            if let Ok(template_cfg) = config::load(&path) {
                                if let Some(host) = template_cfg.host {
                                    host_config = Some(host);
                                    source = Some(path.display().to_string());
                                }
                            }
                        }
                    }

                    if let Some(host) = server.host {
                        host_config = Some(match host_config {
                            Some(existing) => merge_host_config(existing, host),
                            None => host,
                        });
                        source = Some(format!(
                            "{} (inline)",
                            global_path.display()
                        ));
                    }
                }
            }
        }
    }

    if host_config.is_none() {
        let infra_path = PathBuf::from("/Users/nikiv/infra/flow.toml");
        if infra_path.exists() {
            if let Ok(cfg) = config::load(&infra_path) {
                if let Some(host) = cfg.host {
                    host_config = Some(host);
                    source = Some(infra_path.display().to_string());
                }
            }
        }
    }

    host_config.map(|host| ServerSetupTemplate {
        host,
        source: source.unwrap_or_else(|| "unknown".to_string()),
    })
}

fn merge_host_config(
    base: deploy::HostConfig,
    overlay: deploy::HostConfig,
) -> deploy::HostConfig {
    deploy::HostConfig {
        dest: overlay.dest.or(base.dest),
        setup: overlay.setup.or(base.setup),
        run: overlay.run.or(base.run),
        port: overlay.port.or(base.port),
        service: overlay.service.or(base.service),
        env_file: overlay.env_file.or(base.env_file),
        env_source: overlay.env_source.or(base.env_source),
        env_keys: if overlay.env_keys.is_empty() {
            base.env_keys
        } else {
            overlay.env_keys
        },
        env_personal: overlay.env_personal || base.env_personal,
        environment: overlay.environment.or(base.environment),
        service_token: overlay.service_token.or(base.service_token),
        domain: overlay.domain.or(base.domain),
        ssl: overlay.ssl || base.ssl,
    }
}

fn apply_host_overrides(defaults: &mut DeployDefaults, host: &deploy::HostConfig) {
    if let Some(dest) = host.dest.as_deref() {
        defaults.dest = dest.to_string();
    }

    if let Some(run) = host.run.as_deref() {
        defaults.run = Some(run.to_string());
    }

    if let Some(service) = host.service.as_deref() {
        defaults.service = service.to_string();
    }

    if let Some(setup) = host.setup.as_deref() {
        if looks_like_inline_script(setup) {
            defaults.setup_script_content = Some(setup.to_string());
        } else if !setup.trim().is_empty() {
            defaults.setup_path = setup.to_string();
            defaults.setup_script_content = None;
        }
    }

    if let Some(env_file) = host.env_file.as_deref() {
        if !env_file.trim().is_empty() {
            defaults.env_file = Some(env_file.to_string());
        }
    }

    if let Some(port) = host.port {
        defaults.port = Some(port);
    }

    if let Some(domain) = host.domain.as_deref() {
        if !domain.trim().is_empty() {
            defaults.domain = Some(domain.to_string());
        }
    }

    if host.ssl {
        defaults.ssl = true;
    }
}

fn apply_server_template(
    defaults: &mut DeployDefaults,
    template: Option<&ServerSetupTemplate>,
    project_root: &Path,
) {
    let Some(template) = template else {
        return;
    };
    let host = &template.host;

    if let Some(setup) = host.setup.as_ref() {
        if let Some(reason) = setup_script_mismatch_reason(project_root, setup) {
            println!("Warning: skipping template setup script: {reason}");
        } else if looks_like_inline_script(setup) {
            defaults.setup_script_content = Some(setup.to_string());
        } else {
            defaults.setup_path = setup.to_string();
            defaults.setup_script_content = None;
        }
    }

    if defaults.dest.trim().is_empty() {
        if let Some(dest) = host.dest.as_deref() {
            defaults.dest = dest.to_string();
        }
    }
    if defaults.run.is_none() {
        if let Some(run) = host.run.as_deref() {
            defaults.run = Some(run.to_string());
        }
    }
    if defaults.service.trim().is_empty() {
        if let Some(service) = host.service.as_deref() {
            defaults.service = service.to_string();
        }
    }

    if let Some(env_file) = host.env_file.as_ref() {
        if defaults.env_file.is_none() {
            defaults.env_file = Some(env_file.to_string());
        }
    }
    if host.port.is_some() {
        defaults.port = host.port;
    }
    if let Some(domain) = host.domain.as_ref() {
        defaults.domain = Some(domain.to_string());
    }
    if host.ssl {
        defaults.ssl = true;
    }
}

fn looks_like_inline_script(value: &str) -> bool {
    value.contains('\n') || value.trim_start().starts_with("#!") || value.contains("set -e")
}

fn render_host_section(cfg: &HostSetupConfig) -> String {
    let mut out = String::from("[host]\n");
    out.push_str(&format!("dest = \"{}\"\n", toml_escape(&cfg.dest)));
    if let Some(setup) = &cfg.setup {
        out.push_str(&format!("setup = \"{}\"\n", toml_escape(setup)));
    }
    if let Some(run) = &cfg.run {
        out.push_str(&format!("run = \"{}\"\n", toml_escape(run)));
    }
    if let Some(port) = cfg.port {
        out.push_str(&format!("port = {port}\n"));
    }
    if let Some(service) = &cfg.service {
        out.push_str(&format!("service = \"{}\"\n", toml_escape(service)));
    }
    if let Some(env_file) = &cfg.env_file {
        out.push_str(&format!("env_file = \"{}\"\n", toml_escape(env_file)));
    }
    if let Some(domain) = &cfg.domain {
        out.push_str(&format!("domain = \"{}\"\n", toml_escape(domain)));
    }
    if cfg.ssl {
        out.push_str("ssl = true\n");
    }
    out
}

fn has_host_section(content: &str) -> bool {
    content.lines().any(|line| line.trim() == "[host]")
}

fn append_section(content: &str, section: &str) -> String {
    let mut out = content.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(section.trim_end());
    out.push('\n');
    out
}

fn replace_host_section(content: &str, section: &str) -> String {
    let mut lines: Vec<String> = content.lines().map(|line| line.to_string()).collect();
    let had_trailing_newline = content.ends_with('\n');
    let section_lines: Vec<String> = section
        .trim_end()
        .lines()
        .map(|line| line.to_string())
        .collect();

    if let Some(start) = lines.iter().position(|line| line.trim() == "[host]") {
        let end = find_section_end(&lines, start + 1);
        let mut updated = Vec::new();
        updated.extend_from_slice(&lines[..start]);
        updated.extend(section_lines);
        updated.extend_from_slice(&lines[end..]);
        lines = updated;
    } else {
        if !lines.is_empty() && !lines.last().map(|line| line.trim().is_empty()).unwrap_or(false) {
            lines.push(String::new());
        }
        lines.extend(section_lines);
    }

    let mut out = lines.join("\n");
    if had_trailing_newline {
        out.push('\n');
    }
    out
}

fn find_section_end(lines: &[String], start: usize) -> usize {
    for (idx, line) in lines.iter().enumerate().skip(start) {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            return idx;
        }
    }
    lines.len()
}

fn ensure_setup_script(
    project_root: &Path,
    script_path: &str,
    content: &str,
    overwrite: bool,
) -> Result<()> {
    let path = project_root.join(script_path);
    if path.exists() && !overwrite {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, ensure_trailing_newline(content.to_string()))
        .with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms)?;
    }
    if overwrite && path.exists() {
        println!("Updated {}", path.display());
    } else {
        println!("Created {}", path.display());
    }
    Ok(())
}

fn ensure_env_file(
    project_root: &Path,
    env_file: &str,
    env_example: Option<&PathBuf>,
    interactive: bool,
    auto_gitignore: bool,
) -> Result<()> {
    let env_path = project_root.join(env_file);
    if env_path.exists() {
        return Ok(());
    }

    if let Some(example_path) = env_example {
        if example_path.exists() {
            let should_copy = if interactive {
                prompt_yes_no(
                    &format!(
                        "Copy {} to {}?",
                        display_relative(project_root, example_path),
                        env_file
                    ),
                    true,
                )?
            } else {
                true
            };

            if should_copy {
                if let Some(parent) = env_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(example_path, &env_path).with_context(|| {
                    format!(
                        "failed to copy {} to {}",
                        example_path.display(),
                        env_path.display()
                    )
                })?;
                println!("Created {}", env_path.display());
            }
        }
    }

    if env_path.exists() && interactive {
        if prompt_yes_no("Add env file to .gitignore?", true)? {
            add_gitignore_entry(project_root, env_file)?;
        }
    }

    if env_path.exists() && auto_gitignore && !interactive {
        add_gitignore_entry(project_root, env_file)?;
    }

    Ok(())
}

fn add_gitignore_entry(project_root: &Path, entry: &str) -> Result<()> {
    let gitignore_path = project_root.join(".gitignore");
    let mut content = if gitignore_path.exists() {
        fs::read_to_string(&gitignore_path)?
    } else {
        String::new()
    };

    if content.lines().any(|line| line.trim() == entry) {
        return Ok(());
    }

    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if !content.is_empty() && !content.ends_with("\n\n") {
        content.push('\n');
    }
    content.push_str(entry);
    content.push('\n');

    fs::write(&gitignore_path, content)
        .with_context(|| format!("failed to write {}", gitignore_path.display()))?;
    Ok(())
}

fn maybe_configure_deploy_host(auto_mode: bool) -> Result<()> {
    let existing = deploy::load_deploy_config()?.host;
    if existing.is_some() && auto_mode {
        return Ok(());
    }

    let default_conn = existing
        .as_ref()
        .map(|host| format!("{}@{}:{}", host.user, host.host, host.port))
        .or_else(deploy::default_linux_connection_string);

    if auto_mode {
        if let Some(conn_str) = default_conn.as_deref() {
            let conn = deploy::HostConnection::parse(conn_str)?;
            let mut config = deploy::load_deploy_config()?;
            config.host = Some(conn);
            deploy::save_deploy_config(&config)?;
            println!("Configured deploy host: {}", conn_str);
        } else {
            println!("Host not configured. Run `f deploy config`.");
        }
        return Ok(());
    }

    let should_configure = if existing.is_some() {
        prompt_yes_no("Configure deploy host now?", false)?
    } else {
        prompt_yes_no("Configure deploy host now?", true)?
    };

    if !should_configure {
        if existing.is_none() {
            println!("Host not configured. Run `f deploy set-host user@host:port`.");
        }
        return Ok(());
    }

    let prompt = "SSH host (user@host:port)";
    let input = prompt_line(prompt, default_conn.as_deref())?;
    if input.trim().is_empty() {
        if existing.is_none() {
            println!("Host not configured. Run `f deploy set-host user@host:port`.");
        }
        return Ok(());
    }
    let conn = deploy::HostConnection::parse(input.trim())?;
    let mut config = deploy::load_deploy_config()?;
    config.host = Some(conn);
    deploy::save_deploy_config(&config)?;
    println!("Configured deploy host.");
    Ok(())
}

fn guess_project_name(project_root: &Path) -> String {
    if let Some(name) = cargo_package_name(project_root) {
        return name;
    }
    if let Some(name) = package_json_name(project_root) {
        return name;
    }
    project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("app")
        .to_string()
}

fn cargo_package_name(project_root: &Path) -> Option<String> {
    let path = project_root.join("Cargo.toml");
    let content = fs::read_to_string(&path).ok()?;
    let value: toml::Value = toml::from_str(&content).ok()?;
    let name = value
        .get("package")
        .and_then(toml::Value::as_table)
        .and_then(|pkg| pkg.get("name"))
        .and_then(toml::Value::as_str)?;
    Some(name.to_string())
}

fn package_json_name(project_root: &Path) -> Option<String> {
    let path = project_root.join("package.json");
    let content = fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    let name = value.get("name")?.as_str()?;
    Some(strip_scope(name).to_string())
}

fn strip_scope(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

fn default_run_command(project_root: &Path, project_name: &str) -> Option<String> {
    if project_root.join("Cargo.toml").exists() {
        return Some(format!("./target/release/{}", project_name));
    }
    None
}

fn default_setup_script(project_root: &Path) -> String {
    if project_root.join("Cargo.toml").exists() {
        return rust_deploy_setup_script();
    }
    if project_root.join("package.json").exists() {
        return node_deploy_setup_script();
    }
    generic_deploy_setup_script()
}

fn rust_deploy_setup_script() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  if [ -f "$HOME/.cargo/env" ]; then
    . "$HOME/.cargo/env"
  fi
fi

cargo build --release
"#
    .to_string()
}

fn node_deploy_setup_script() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

if [ -f pnpm-lock.yaml ]; then
  pnpm install
elif [ -f yarn.lock ]; then
  yarn install
elif [ -f bun.lockb ]; then
  bun install
elif [ -f package-lock.json ]; then
  npm ci
else
  npm install
fi

npm run build
"#
    .to_string()
}

fn generic_deploy_setup_script() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

echo "TODO: add remote setup steps"
"#
    .to_string()
}

fn find_env_example(project_root: &Path, project_name: &str) -> Option<PathBuf> {
    let candidates = [
        format!("deploy/{}.env.example", project_name),
        "deploy/.env.example".to_string(),
        ".env.example".to_string(),
    ];
    for candidate in candidates {
        let path = project_root.join(&candidate);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn strip_example_suffix(project_root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(project_root).ok()?;
    let rel_str = rel.to_string_lossy();
    let trimmed = rel_str.strip_suffix(".example")?;
    Some(trimmed.to_string())
}

fn display_relative(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
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

    if let Some(guidance) = project_guidance(project_root) {
        prompt.push_str("Guidance:\n");
        prompt.push_str(&guidance);
        prompt.push('\n');
    }

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

fn generate_flow_toml_with_agent_streaming(
    project_root: &Path,
    hint: Option<&str>,
) -> Result<String> {
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

    if let Some(guidance) = project_guidance(project_root) {
        prompt.push_str("Guidance:\n");
        prompt.push_str(&guidance);
        prompt.push('\n');
    }

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

    agents::run_flow_agent_capture_streaming(&prompt)
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

#[derive(Deserialize)]
struct HostWrapper {
    host: Option<deploy::HostConfig>,
}

fn generate_host_config_with_agent(project_root: &Path, hint: Option<&str>) -> Result<String> {
    let defaults = deploy_defaults(project_root);
    let mut prompt = String::new();
    prompt.push_str("Read the project and generate a minimal [host] config for flow.toml.\n");
    prompt.push_str("Requirements:\n");
    prompt.push_str("- Output ONLY TOML with a [host] section.\n");
    prompt.push_str("- No explanations, no narration, no markdown fences.\n");
    prompt.push_str("- Use relative paths for setup/env_file.\n");
    prompt.push_str("- Use a production run command (avoid dev servers).\n");
    prompt.push_str("- Keep it minimal; omit fields you cannot infer.\n\n");

    prompt.push_str("Suggested defaults:\n");
    prompt.push_str(&format!("- dest: {}\n", defaults.dest));
    if let Some(run) = defaults.run.as_deref() {
        prompt.push_str(&format!("- run: {}\n", run));
    }
    prompt.push_str(&format!("- service: {}\n", defaults.service));
    if !defaults.setup_path.trim().is_empty() {
        prompt.push_str(&format!("- setup: {}\n", defaults.setup_path));
    }
    if let Some(env_file) = defaults.env_file.as_deref() {
        prompt.push_str(&format!("- env_file: {}\n", env_file));
    }
    if let Some(env_example) = defaults.env_example.as_ref() {
        prompt.push_str(&format!(
            "- env example: {}\n",
            display_relative(project_root, env_example)
        ));
    }
    if let Some(port) = defaults.port {
        prompt.push_str(&format!("- port: {}\n", port));
    }
    prompt.push('\n');

    if let Some(guidance) = project_guidance(project_root) {
        prompt.push_str("Guidance:\n");
        prompt.push_str(&guidance);
        prompt.push('\n');
    }

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

fn extract_host_config(raw: &str) -> Option<deploy::HostConfig> {
    let content = extract_fenced_block(raw, "toml")
        .or_else(|| extract_fenced_block(raw, ""))
        .unwrap_or_else(|| raw.trim().to_string());

    if content.trim().is_empty() {
        return None;
    }

    if content.contains("[host]") {
        if let Ok(wrapper) = toml::from_str::<HostWrapper>(&content) {
            if let Some(host) = wrapper.host {
                if host_has_values(&host) {
                    return Some(host);
                }
            }
        }
    } else if let Ok(host) = toml::from_str::<deploy::HostConfig>(&content) {
        if host_has_values(&host) {
            return Some(host);
        }
    }

    None
}

fn host_has_values(host: &deploy::HostConfig) -> bool {
    host.dest.is_some()
        || host.setup.is_some()
        || host.run.is_some()
        || host.port.is_some()
        || host.service.is_some()
        || host.env_file.is_some()
        || host.domain.is_some()
        || host.ssl
}

fn host_config_mismatch_reason(
    project_root: &Path,
    host_cfg: &deploy::HostConfig,
) -> Option<String> {
    let has_cargo = project_root.join("Cargo.toml").exists();
    let has_package = project_root.join("package.json").exists();

    let mut uses_node = false;
    let mut uses_cargo = false;

    if let Some(run) = host_cfg.run.as_deref() {
        uses_node |= command_uses_node_tool(run);
        uses_cargo |= command_uses_cargo_tool(run);
    }

    if let Some(setup) = host_cfg.setup.as_deref() {
        if looks_like_inline_script(setup) {
            uses_node |= command_uses_node_tool(setup);
            uses_cargo |= command_uses_cargo_tool(setup);
        } else {
            let setup_path = project_root.join(setup);
            if setup_path.exists() {
                if let Ok(content) = fs::read_to_string(&setup_path) {
                    uses_node |= command_uses_node_tool(&content);
                    uses_cargo |= command_uses_cargo_tool(&content);
                }
            }
        }
    }

    if has_cargo && !has_package && uses_node {
        return Some(
            "AI suggested Node tooling (bun/npm/pnpm/yarn), but no package.json was found."
                .to_string(),
        );
    }
    if has_package && !has_cargo && uses_cargo {
        return Some("AI suggested Cargo commands, but no Cargo.toml was found.".to_string());
    }

    if let Some(reason) = host_config_name_mismatch(project_root, host_cfg) {
        return Some(reason);
    }

    None
}

fn command_mismatch_reason(project_root: &Path, command: &str) -> Option<String> {
    let has_cargo = project_root.join("Cargo.toml").exists();
    let has_package = project_root.join("package.json").exists();

    let uses_node = command_uses_node_tool(command);
    let uses_cargo = command_uses_cargo_tool(command);

    if has_cargo && !has_package && uses_node {
        return Some(
            "uses Node tooling but no package.json was found for this project.".to_string(),
        );
    }
    if has_package && !has_cargo && uses_cargo {
        return Some("uses Cargo but no Cargo.toml was found for this project.".to_string());
    }

    None
}

fn setup_script_mismatch_reason(project_root: &Path, setup: &str) -> Option<String> {
    let has_cargo = project_root.join("Cargo.toml").exists();
    let has_package = project_root.join("package.json").exists();

    let mut uses_node = false;
    let mut uses_cargo = false;

    if looks_like_inline_script(setup) {
        uses_node |= command_uses_node_tool(setup);
        uses_cargo |= command_uses_cargo_tool(setup);
    } else {
        let setup_path = project_root.join(setup);
        if setup_path.exists() {
            if let Ok(content) = fs::read_to_string(&setup_path) {
                uses_node |= command_uses_node_tool(&content);
                uses_cargo |= command_uses_cargo_tool(&content);
            }
        }
    }

    if has_cargo && !has_package && uses_node {
        return Some(
            "uses Node tooling but no package.json was found for this project.".to_string(),
        );
    }
    if has_package && !has_cargo && uses_cargo {
        return Some("uses Cargo but no Cargo.toml was found for this project.".to_string());
    }

    None
}

fn host_config_name_mismatch(
    project_root: &Path,
    host_cfg: &deploy::HostConfig,
) -> Option<String> {
    let expected_names = expected_project_names(project_root);
    if expected_names.is_empty() {
        return None;
    }

    let tokens = host_name_tokens(host_cfg);
    if tokens.is_empty() {
        return None;
    }

    let mut counts: HashMap<String, usize> = HashMap::new();
    for token in tokens {
        if expected_names.contains(&token) {
            continue;
        }
        *counts.entry(token).or_insert(0) += 1;
    }

    let (token, count) = counts
        .into_iter()
        .max_by_key(|(_, count)| *count)?;
    if count < 2 {
        return None;
    }

    let project_name = guess_project_name(project_root);
    Some(format!(
        "AI suggested host config for '{}', but the project looks like '{}'.",
        token, project_name
    ))
}

fn expected_project_names(project_root: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    let guessed = guess_project_name(project_root);
    if !guessed.is_empty() {
        names.insert(guessed.to_ascii_lowercase());
    }
    if let Some(name) = cargo_package_name(project_root) {
        names.insert(name.to_ascii_lowercase());
    }
    if let Some(name) = package_json_name(project_root) {
        names.insert(name.to_ascii_lowercase());
    }
    if let Some(folder) = project_root
        .file_name()
        .and_then(|name| name.to_str())
    {
        names.insert(folder.to_ascii_lowercase());
    }
    for name in cargo_bin_names(project_root) {
        names.insert(name);
    }
    names
}

fn cargo_bin_names(project_root: &Path) -> Vec<String> {
    let path = project_root.join("Cargo.toml");
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(_) => return Vec::new(),
    };
    let value: toml::Value = match toml::from_str(&content) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };

    let mut names = Vec::new();
    if let Some(bins) = value.get("bin").and_then(toml::Value::as_array) {
        for bin in bins {
            if let Some(name) = bin.get("name").and_then(toml::Value::as_str) {
                names.push(name.to_ascii_lowercase());
            }
        }
    }
    names
}

fn host_name_tokens(host: &deploy::HostConfig) -> Vec<String> {
    let mut tokens = Vec::new();

    if let Some(service) = host.service.as_deref() {
        if let Some(token) = normalize_host_token(service) {
            tokens.push(token);
        }
    }

    if let Some(dest) = host.dest.as_deref() {
        if let Some(seg) = Path::new(dest).file_name().and_then(|s| s.to_str()) {
            if let Some(token) = normalize_host_token(seg) {
                tokens.push(token);
            }
        }
    }

    if let Some(run) = host.run.as_deref() {
        if let Some(bin) = extract_run_binary(run) {
            if let Some(token) = normalize_host_token(&bin) {
                tokens.push(token);
            }
        }
    }

    if let Some(env_file) = host.env_file.as_deref() {
        if let Some(env_name) = extract_env_name(env_file) {
            if let Some(token) = normalize_host_token(&env_name) {
                tokens.push(token);
            }
        }
    }

    tokens
}

fn extract_run_binary(run: &str) -> Option<String> {
    let first = run.trim().split_whitespace().next()?;
    let trimmed = first.trim_matches(|c| c == '"' || c == '\'');
    let name = Path::new(trimmed).file_name()?.to_string_lossy().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn extract_env_name(env_file: &str) -> Option<String> {
    let file_name = Path::new(env_file).file_name()?.to_string_lossy();
    if file_name.starts_with('.') {
        return None;
    }
    let mut stem = Path::new(&*file_name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())?;
    if let Some(stripped) = stem.strip_suffix(".env") {
        stem = stripped.to_string();
    }
    if stem.is_empty() {
        None
    } else {
        Some(stem)
    }
}

fn normalize_host_token(token: &str) -> Option<String> {
    let trimmed = token
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
    if trimmed.len() < 2 {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if is_host_stop_token(&lower) {
        None
    } else {
        Some(lower)
    }
}

fn is_host_stop_token(token: &str) -> bool {
    matches!(
        token,
        "app"
            | "service"
            | "server"
            | "api"
            | "web"
            | "backend"
            | "frontend"
            | "bin"
            | "target"
            | "release"
            | "debug"
            | "dist"
            | "build"
            | "deploy"
            | "env"
            | "cargo"
            | "bun"
            | "npm"
            | "pnpm"
            | "yarn"
            | "node"
    )
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

fn project_guidance(project_root: &Path) -> Option<String> {
    let has_cargo = project_root.join("Cargo.toml").exists();
    let has_package = project_root.join("package.json").exists();

    match (has_cargo, has_package) {
        (true, false) => Some("Detected Rust project (Cargo.toml). Use cargo commands; avoid bun/npm/pnpm/yarn.".to_string()),
        (false, true) => Some("Detected Node project (package.json). Use npm/pnpm/yarn/bun commands; avoid cargo.".to_string()),
        (true, true) => Some("Detected Rust + Node (Cargo.toml + package.json). Use the right tool for each step.".to_string()),
        _ => None,
    }
}

fn detect_server_project(project_root: &Path) -> Option<String> {
    if let Some(reason) = detect_rust_server(project_root) {
        return Some(reason);
    }
    if let Some(reason) = detect_node_server(project_root) {
        return Some(reason);
    }
    None
}

fn detect_rust_server(project_root: &Path) -> Option<String> {
    let path = project_root.join("Cargo.toml");
    let content = fs::read_to_string(&path).ok()?;
    let value: toml::Value = toml::from_str(&content).ok()?;

    let mut deps = std::collections::HashSet::new();
    if let Some(table) = value.get("dependencies").and_then(toml::Value::as_table) {
        deps.extend(table.keys().cloned());
    }
    if let Some(workspace) = value.get("workspace").and_then(toml::Value::as_table) {
        if let Some(table) = workspace
            .get("dependencies")
            .and_then(toml::Value::as_table)
        {
            deps.extend(table.keys().cloned());
        }
    }

    let server_deps = [
        "axum",
        "actix-web",
        "warp",
        "rocket",
        "hyper",
        "tower-http",
        "tonic",
    ];
    for dep in server_deps {
        if deps.contains(dep) {
            return Some(format!("Rust server crate detected: {dep}"));
        }
    }

    None
}

fn detect_node_server(project_root: &Path) -> Option<String> {
    let path = project_root.join("package.json");
    let content = fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;

    let mut deps = std::collections::HashSet::new();
    for key in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(table) = value.get(key).and_then(|v| v.as_object()) {
            deps.extend(table.keys().cloned());
        }
    }

    let server_deps = ["express", "fastify", "koa", "hono", "next", "remix", "nestjs"];
    for dep in server_deps {
        if deps.contains(dep) {
            return Some(format!("Node server framework detected: {dep}"));
        }
    }

    None
}

fn ai_flow_toml_mismatch_reason(project_root: &Path, toml_content: &str) -> Option<String> {
    let has_cargo = project_root.join("Cargo.toml").exists();
    let has_package = project_root.join("package.json").exists();
    let parsed: toml::Value = toml::from_str(toml_content).ok()?;

    let tasks = parsed.get("tasks").and_then(toml::Value::as_array)?;

    let mut uses_node = false;
    let mut uses_cargo = false;

    for task in tasks {
        let command = match task.get("command").and_then(toml::Value::as_str) {
            Some(cmd) => cmd,
            None => continue,
        };
        uses_node |= command_uses_node_tool(command);
        uses_cargo |= command_uses_cargo_tool(command);
    }

    if has_cargo && !has_package && uses_node {
        return Some("AI suggested Node tooling (bun/npm/pnpm/yarn), but no package.json was found.".to_string());
    }
    if has_package && !has_cargo && uses_cargo {
        return Some("AI suggested Cargo commands, but no Cargo.toml was found.".to_string());
    }

    None
}

fn command_uses_node_tool(command: &str) -> bool {
    ["bun", "npm", "pnpm", "yarn"]
        .iter()
        .any(|tool| command_mentions_tool(command, tool))
}

fn command_uses_cargo_tool(command: &str) -> bool {
    command_mentions_tool(command, "cargo")
}

fn command_mentions_tool(command: &str, tool: &str) -> bool {
    command.split_whitespace().any(|part| {
        let trimmed = part
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
        trimmed.eq_ignore_ascii_case(tool)
    })
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

fn prompt_line_optional(message: &str, default: Option<&str>) -> Result<Option<String>> {
    let value = prompt_line(message, default)?;
    Ok(normalize_optional(value))
}

fn prompt_u16_optional(message: &str, default: Option<u16>) -> Result<Option<u16>> {
    let default_str = default.map(|v| v.to_string());
    let value = prompt_line_optional(message, default_str.as_deref())?;
    match value {
        Some(text) => text
            .parse::<u16>()
            .map(Some)
            .context("invalid port value"),
        None => Ok(None),
    }
}

fn normalize_optional(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
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
