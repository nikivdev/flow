//! macOS launchd service management.
//!
//! Provides tools to list, audit, enable, and disable macOS launch agents and daemons.
//! Helps keep the system clean by identifying bloatware and unwanted background processes.

use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::cli::{
    MacosAction, MacosAuditOpts, MacosCleanOpts, MacosCommand, MacosDisableOpts, MacosEnableOpts,
    MacosInfoOpts, MacosListOpts,
};
use crate::config::{self, MacosConfig};

/// Service location type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceType {
    /// ~/Library/LaunchAgents
    UserAgent,
    /// /Library/LaunchAgents
    SystemAgent,
    /// /Library/LaunchDaemons
    SystemDaemon,
}

impl ServiceType {
    fn as_str(&self) -> &'static str {
        match self {
            ServiceType::UserAgent => "user-agent",
            ServiceType::SystemAgent => "system-agent",
            ServiceType::SystemDaemon => "system-daemon",
        }
    }

    fn requires_sudo(&self) -> bool {
        matches!(self, ServiceType::SystemAgent | ServiceType::SystemDaemon)
    }

    fn domain(&self) -> String {
        match self {
            ServiceType::UserAgent => format!("gui/{}", get_uid()),
            ServiceType::SystemAgent | ServiceType::SystemDaemon => "system".to_string(),
        }
    }
}

/// Service category for classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceCategory {
    Apple,
    Custom,
    Database,
    Docker,
    Vpn,
    Ai,
    Bloatware,
    Development,
    Unknown,
}

impl ServiceCategory {
    fn as_str(&self) -> &'static str {
        match self {
            ServiceCategory::Apple => "apple",
            ServiceCategory::Custom => "custom",
            ServiceCategory::Database => "database",
            ServiceCategory::Docker => "docker",
            ServiceCategory::Vpn => "vpn",
            ServiceCategory::Ai => "ai",
            ServiceCategory::Bloatware => "bloatware",
            ServiceCategory::Development => "development",
            ServiceCategory::Unknown => "unknown",
        }
    }
}

/// Represents a discovered launchd service.
#[derive(Debug, Clone, Serialize)]
pub struct LaunchdService {
    /// Service identifier (e.g., com.apple.Finder).
    pub id: String,
    /// Path to the plist file.
    pub plist_path: PathBuf,
    /// Whether the service is currently loaded.
    pub loaded: bool,
    /// Whether the service is currently running.
    pub running: bool,
    /// Process ID if running.
    pub pid: Option<u32>,
    /// Service type (user agent, system agent, system daemon).
    pub service_type: ServiceType,
    /// Service category.
    pub category: ServiceCategory,
    /// Program or ProgramArguments from plist.
    pub program: Option<String>,
}

/// Audit recommendation for a service.
#[derive(Debug, Clone, Serialize)]
pub struct AuditRecommendation {
    pub service: LaunchdService,
    pub action: RecommendedAction,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedAction {
    Keep,
    Disable,
    Review,
}

pub fn run(cmd: MacosCommand) -> Result<()> {
    match cmd.action {
        Some(MacosAction::List(opts)) => run_list(opts),
        Some(MacosAction::Status) => run_status(),
        Some(MacosAction::Audit(opts)) => run_audit(opts),
        Some(MacosAction::Info(opts)) => run_info(opts),
        Some(MacosAction::Disable(opts)) => run_disable(opts),
        Some(MacosAction::Enable(opts)) => run_enable(opts),
        Some(MacosAction::Clean(opts)) => run_clean(opts),
        None => run_status(),
    }
}

fn run_list(opts: MacosListOpts) -> Result<()> {
    let services = discover_services()?;

    let filtered: Vec<_> = services
        .into_iter()
        .filter(|s| {
            if opts.user && !matches!(s.service_type, ServiceType::UserAgent) {
                return false;
            }
            if opts.system && matches!(s.service_type, ServiceType::UserAgent) {
                return false;
            }
            true
        })
        .collect();

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&filtered)?);
        return Ok(());
    }

    println!("Discovered {} services\n", filtered.len());

    // Group by type
    let mut by_type: HashMap<&str, Vec<&LaunchdService>> = HashMap::new();
    for svc in &filtered {
        by_type
            .entry(svc.service_type.as_str())
            .or_default()
            .push(svc);
    }

    for (type_name, services) in by_type.iter() {
        println!("{}:", type_name);
        for svc in services {
            let status = if svc.running {
                format!("running (pid {})", svc.pid.unwrap_or(0))
            } else if svc.loaded {
                "loaded".to_string()
            } else {
                "disabled".to_string()
            };
            println!(
                "  {} [{}] - {}",
                svc.id,
                svc.category.as_str(),
                status
            );
        }
        println!();
    }

    Ok(())
}

fn run_status() -> Result<()> {
    let services = discover_services()?;

    // Filter to non-Apple running services
    let running: Vec<_> = services
        .iter()
        .filter(|s| s.running && s.category != ServiceCategory::Apple)
        .collect();

    if running.is_empty() {
        println!("No non-Apple services currently running.");
        return Ok(());
    }

    println!("Running non-Apple services:\n");
    for svc in running {
        let pid_str = svc
            .pid
            .map(|p| format!(" (pid {})", p))
            .unwrap_or_default();
        println!(
            "  {} [{}]{}",
            svc.id,
            svc.category.as_str(),
            pid_str
        );
        if let Some(prog) = &svc.program {
            println!("    {}", prog);
        }
    }

    Ok(())
}

fn run_audit(opts: MacosAuditOpts) -> Result<()> {
    let services = discover_services()?;
    let macos_config = load_macos_config();
    let recommendations = audit_services(&services, &macos_config);

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&recommendations)?);
        return Ok(());
    }

    let to_disable: Vec<_> = recommendations
        .iter()
        .filter(|r| r.action == RecommendedAction::Disable)
        .collect();
    let to_review: Vec<_> = recommendations
        .iter()
        .filter(|r| r.action == RecommendedAction::Review)
        .collect();

    if to_disable.is_empty() && to_review.is_empty() {
        println!("All services look good! No recommendations.");
        return Ok(());
    }

    if !to_disable.is_empty() {
        println!("Recommended to DISABLE ({}):\n", to_disable.len());
        for rec in &to_disable {
            println!("  {} [{}]", rec.service.id, rec.service.category.as_str());
            println!("    Reason: {}", rec.reason);
        }
        println!();
    }

    if !to_review.is_empty() {
        println!("Recommended to REVIEW ({}):\n", to_review.len());
        for rec in &to_review {
            println!("  {} [{}]", rec.service.id, rec.service.category.as_str());
            println!("    Reason: {}", rec.reason);
        }
        println!();
    }

    if !to_disable.is_empty() {
        println!(
            "Run `f macos clean` to disable {} bloatware services.",
            to_disable.len()
        );
    }

    Ok(())
}

fn run_info(opts: MacosInfoOpts) -> Result<()> {
    let services = discover_services()?;

    let svc = services
        .iter()
        .find(|s| s.id == opts.service)
        .ok_or_else(|| anyhow::anyhow!("Service '{}' not found", opts.service))?;

    println!("Service: {}", svc.id);
    println!("Type:    {}", svc.service_type.as_str());
    println!("Category:{}", svc.category.as_str());
    println!("Plist:   {}", svc.plist_path.display());
    println!("Loaded:  {}", svc.loaded);
    println!("Running: {}", svc.running);
    if let Some(pid) = svc.pid {
        println!("PID:     {}", pid);
    }
    if let Some(prog) = &svc.program {
        println!("Program: {}", prog);
    }

    // Show plist content
    println!("\nPlist contents:");
    if let Ok(content) = std::fs::read_to_string(&svc.plist_path) {
        println!("{}", content);
    }

    Ok(())
}

fn run_disable(opts: MacosDisableOpts) -> Result<()> {
    let services = discover_services()?;

    let svc = services
        .iter()
        .find(|s| s.id == opts.service)
        .ok_or_else(|| anyhow::anyhow!("Service '{}' not found", opts.service))?;

    if svc.category == ServiceCategory::Apple {
        bail!(
            "Refusing to disable Apple service '{}'. This could break your system.",
            svc.id
        );
    }

    if !opts.yes {
        print!(
            "Disable service '{}'? [y/N] ",
            svc.id
        );
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    disable_service(svc)?;
    println!("Disabled service '{}'", svc.id);

    Ok(())
}

fn run_enable(opts: MacosEnableOpts) -> Result<()> {
    let services = discover_services()?;

    let svc = services
        .iter()
        .find(|s| s.id == opts.service)
        .ok_or_else(|| anyhow::anyhow!("Service '{}' not found", opts.service))?;

    enable_service(svc)?;
    println!("Enabled service '{}'", svc.id);

    Ok(())
}

fn run_clean(opts: MacosCleanOpts) -> Result<()> {
    let services = discover_services()?;
    let macos_config = load_macos_config();
    let recommendations = audit_services(&services, &macos_config);

    let to_disable: Vec<_> = recommendations
        .iter()
        .filter(|r| r.action == RecommendedAction::Disable)
        .collect();

    if to_disable.is_empty() {
        println!("No bloatware services found to clean.");
        return Ok(());
    }

    println!("Services to disable ({}):\n", to_disable.len());
    for rec in &to_disable {
        println!("  {} - {}", rec.service.id, rec.reason);
    }
    println!();

    if opts.dry_run {
        println!("Dry run - no changes made.");
        return Ok(());
    }

    if !opts.yes && io::stdin().is_terminal() {
        print!("Disable these {} services? [y/N] ", to_disable.len());
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let mut disabled = 0;
    let mut failed = 0;
    for rec in to_disable {
        match disable_service(&rec.service) {
            Ok(()) => {
                println!("Disabled: {}", rec.service.id);
                disabled += 1;
            }
            Err(e) => {
                eprintln!("Failed to disable {}: {}", rec.service.id, e);
                failed += 1;
            }
        }
    }

    println!("\nDisabled {} services, {} failed.", disabled, failed);

    Ok(())
}

/// Discover all launchd services from the standard locations.
fn discover_services() -> Result<Vec<LaunchdService>> {
    let mut services = Vec::new();

    // User agents
    if let Some(home) = dirs::home_dir() {
        let user_agents = home.join("Library/LaunchAgents");
        if user_agents.exists() {
            discover_in_dir(&user_agents, ServiceType::UserAgent, &mut services)?;
        }
    }

    // System agents
    let system_agents = Path::new("/Library/LaunchAgents");
    if system_agents.exists() {
        discover_in_dir(system_agents, ServiceType::SystemAgent, &mut services)?;
    }

    // System daemons
    let system_daemons = Path::new("/Library/LaunchDaemons");
    if system_daemons.exists() {
        discover_in_dir(system_daemons, ServiceType::SystemDaemon, &mut services)?;
    }

    // Enrich with launchctl status
    enrich_with_launchctl_status(&mut services);

    Ok(services)
}

fn discover_in_dir(
    dir: &Path,
    service_type: ServiceType,
    services: &mut Vec<LaunchdService>,
) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read directory: {}", dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "plist").unwrap_or(false) {
            if let Some(svc) = parse_plist(&path, service_type) {
                services.push(svc);
            }
        }
    }

    Ok(())
}

/// Parse a plist file and extract service information.
fn parse_plist(path: &Path, service_type: ServiceType) -> Option<LaunchdService> {
    // Use plutil to convert to JSON
    let output = Command::new("plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;

    let label = json.get("Label")?.as_str()?.to_string();
    let program = json
        .get("Program")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            json.get("ProgramArguments")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });

    let category = categorize_service(&label);

    Some(LaunchdService {
        id: label,
        plist_path: path.to_path_buf(),
        loaded: false,
        running: false,
        pid: None,
        service_type,
        category,
        program,
    })
}

/// Enrich services with launchctl status information.
fn enrich_with_launchctl_status(services: &mut [LaunchdService]) {
    // Query user domain
    let uid = get_uid();
    if let Ok(output) = Command::new("launchctl")
        .args(["list"])
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            parse_launchctl_list(&stdout, services);
        }
    }

    // For system services, we need to check separately
    // (requires sudo for full info, but we can get some info without)
    if let Ok(_output) = Command::new("launchctl")
        .args(["print", &format!("gui/{}", uid)])
        .output()
    {
        // Parse the print output for more detailed status
        // (This is optional and provides additional detail)
    }
}

fn parse_launchctl_list(output: &str, services: &mut [LaunchdService]) {
    for line in output.lines().skip(1) {
        // Format: PID Status Label
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            let pid_str = parts[0];
            let label = parts[2];

            if let Some(svc) = services.iter_mut().find(|s| s.id == label) {
                svc.loaded = true;
                if pid_str != "-" {
                    if let Ok(pid) = pid_str.parse::<u32>() {
                        svc.running = true;
                        svc.pid = Some(pid);
                    }
                }
            }
        }
    }
}

/// Categorize a service based on its identifier.
fn categorize_service(label: &str) -> ServiceCategory {
    if label.starts_with("com.apple.") {
        return ServiceCategory::Apple;
    }

    // Known bloatware patterns
    if is_known_bloatware(label) {
        return ServiceCategory::Bloatware;
    }

    // Database services
    if label.contains("postgres")
        || label.contains("mysql")
        || label.contains("redis")
        || label.contains("mongo")
    {
        return ServiceCategory::Database;
    }

    // Docker
    if label.contains("docker") || label.contains("orbstack") {
        return ServiceCategory::Docker;
    }

    // VPN
    if label.contains("vpn")
        || label.contains("wireguard")
        || label.contains("tailscale")
        || label.contains("nordvpn")
    {
        return ServiceCategory::Vpn;
    }

    // AI tools
    if label.contains("lmstudio")
        || label.contains("ollama")
        || label.contains("copilot")
    {
        return ServiceCategory::Ai;
    }

    // Development
    if label.contains("homebrew")
        || label.contains("nix")
        || label.contains("watchman")
        || label.contains("github")
    {
        return ServiceCategory::Development;
    }

    ServiceCategory::Unknown
}

/// Check if a service is known bloatware.
fn is_known_bloatware(label: &str) -> bool {
    let bloatware_patterns = [
        "com.google.keystone",
        "com.google.GoogleUpdater",
        "com.adobe.ARMDC",
        "com.adobe.ARMDCHelper",
        "com.adobe.AdobeCreativeCloud",
        "com.adobe.acc",
        "us.zoom.ZoomDaemon",
        "us.zoom.updater",
        "com.microsoft.update",
        "com.microsoft.autoupdate",
        "com.dropbox.",
        "com.spotify.webhelper",
        "com.valvesoftware.steam",
        "com.skype.",
        "com.slack.update",
    ];

    for pattern in bloatware_patterns {
        if label.starts_with(pattern) || label.contains(pattern) {
            return true;
        }
    }

    false
}

/// Audit services and generate recommendations.
fn audit_services(
    services: &[LaunchdService],
    config: &Option<MacosConfig>,
) -> Vec<AuditRecommendation> {
    let mut recommendations = Vec::new();

    for svc in services {
        // Skip Apple services
        if svc.category == ServiceCategory::Apple {
            continue;
        }

        // Check if explicitly allowed
        if let Some(cfg) = config {
            if is_pattern_match(&svc.id, &cfg.allowed) {
                continue;
            }
        }

        // Check if explicitly blocked
        if let Some(cfg) = config {
            if is_pattern_match(&svc.id, &cfg.blocked) {
                recommendations.push(AuditRecommendation {
                    service: svc.clone(),
                    action: RecommendedAction::Disable,
                    reason: "Matched blocked pattern in flow.toml".to_string(),
                });
                continue;
            }
        }

        // Known bloatware
        if svc.category == ServiceCategory::Bloatware {
            recommendations.push(AuditRecommendation {
                service: svc.clone(),
                action: RecommendedAction::Disable,
                reason: "Known bloatware/updater service".to_string(),
            });
            continue;
        }

        // Unknown services that are running
        if svc.category == ServiceCategory::Unknown && svc.running {
            recommendations.push(AuditRecommendation {
                service: svc.clone(),
                action: RecommendedAction::Review,
                reason: "Unknown running service".to_string(),
            });
        }
    }

    recommendations
}

/// Check if a label matches any of the patterns.
fn is_pattern_match(label: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        if pattern.ends_with('*') {
            let prefix = &pattern[..pattern.len() - 1];
            if label.starts_with(prefix) {
                return true;
            }
        } else if label == pattern {
            return true;
        }
    }
    false
}

/// Disable a launchd service.
fn disable_service(svc: &LaunchdService) -> Result<()> {
    let domain = svc.service_type.domain();

    // First bootout (unload)
    if svc.loaded {
        let target = format!("{}/{}", domain, svc.id);
        let mut cmd = if svc.service_type.requires_sudo() {
            let mut c = Command::new("sudo");
            c.args(["launchctl", "bootout", &target]);
            c
        } else {
            let mut c = Command::new("launchctl");
            c.args(["bootout", &target]);
            c
        };

        let output = cmd.output()?;
        if !output.status.success() {
            // Bootout may fail if not loaded, continue to disable
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("No such process") && !stderr.contains("Could not find service") {
                // Log but don't fail - the service might already be unloaded
                tracing::debug!("bootout warning: {}", stderr);
            }
        }
    }

    // Then disable
    let target = format!("{}/{}", domain, svc.id);
    let mut cmd = if svc.service_type.requires_sudo() {
        let mut c = Command::new("sudo");
        c.args(["launchctl", "disable", &target]);
        c
    } else {
        let mut c = Command::new("launchctl");
        c.args(["disable", &target]);
        c
    };

    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to disable service: {}", stderr);
    }

    Ok(())
}

/// Enable a launchd service.
fn enable_service(svc: &LaunchdService) -> Result<()> {
    let domain = svc.service_type.domain();

    // First enable
    let target = format!("{}/{}", domain, svc.id);
    let mut cmd = if svc.service_type.requires_sudo() {
        let mut c = Command::new("sudo");
        c.args(["launchctl", "enable", &target]);
        c
    } else {
        let mut c = Command::new("launchctl");
        c.args(["enable", &target]);
        c
    };

    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to enable service: {}", stderr);
    }

    // Then bootstrap (load)
    let mut cmd = if svc.service_type.requires_sudo() {
        let mut c = Command::new("sudo");
        c.args(["launchctl", "bootstrap", &domain]);
        c.arg(&svc.plist_path);
        c
    } else {
        let mut c = Command::new("launchctl");
        c.args(["bootstrap", &domain]);
        c.arg(&svc.plist_path);
        c
    };

    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Bootstrap may fail if already loaded
        if !stderr.contains("already loaded") && !stderr.contains("service already loaded") {
            bail!("Failed to bootstrap service: {}", stderr);
        }
    }

    Ok(())
}

/// Get the current user's UID.
fn get_uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Load macOS config from global flow.toml.
fn load_macos_config() -> Option<MacosConfig> {
    let config_path = config::default_config_path();
    if !config_path.exists() {
        return None;
    }

    let cfg = config::load_or_default(&config_path);
    cfg.macos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_categorize_apple() {
        assert_eq!(
            categorize_service("com.apple.Finder"),
            ServiceCategory::Apple
        );
    }

    #[test]
    fn test_categorize_bloatware() {
        assert_eq!(
            categorize_service("com.google.keystone.agent"),
            ServiceCategory::Bloatware
        );
        assert_eq!(
            categorize_service("com.adobe.ARMDC.Agent"),
            ServiceCategory::Bloatware
        );
    }

    #[test]
    fn test_is_known_bloatware() {
        assert!(is_known_bloatware("com.google.keystone.agent"));
        assert!(is_known_bloatware("com.adobe.ARMDCHelper.plist"));
        assert!(is_known_bloatware("us.zoom.ZoomDaemon"));
        assert!(!is_known_bloatware("com.apple.Finder"));
    }

    #[test]
    fn test_pattern_match() {
        let patterns = vec![
            "com.nikiv.*".to_string(),
            "exact.match".to_string(),
        ];

        assert!(is_pattern_match("com.nikiv.service", &patterns));
        assert!(is_pattern_match("com.nikiv.other", &patterns));
        assert!(is_pattern_match("exact.match", &patterns));
        assert!(!is_pattern_match("com.other.service", &patterns));
    }
}
