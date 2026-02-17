use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::Deserialize;

use crate::cli::{InstallBackend, InstallIndexOpts, InstallOpts};
use crate::config::FloxInstallSpec;
use crate::registry;

pub fn run(mut opts: InstallOpts) -> Result<()> {
    if opts
        .name
        .as_deref()
        .map(|name| name.trim().is_empty())
        .unwrap_or(true)
    {
        opts.backend = InstallBackend::Flox;
        opts.name = Some(prompt_flox_package()?);
    }

    match opts.backend {
        InstallBackend::Registry => registry::install(opts),
        InstallBackend::Flox => install_with_flox(&opts),
        InstallBackend::Parm => install_with_parm(&opts),
        InstallBackend::Auto => install_with_auto(&opts),
    }
}

fn install_with_auto(opts: &InstallOpts) -> Result<()> {
    let mut errors: Vec<String> = Vec::new();

    if registry_configured(opts) {
        match registry::install(opts.clone()) {
            Ok(()) => return Ok(()),
            Err(err) => {
                eprintln!("WARN registry install failed: {err}");
                errors.push(format!("registry: {err}"));
            }
        }
    }

    if should_try_parm(opts) {
        match install_with_parm(opts) {
            Ok(()) => return Ok(()),
            Err(err) => {
                eprintln!("WARN parm install failed: {err}");
                errors.push(format!("parm: {err}"));
            }
        }
    } else if let Some(name) = opts.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        eprintln!(
            "INFO skipping parm fallback for '{}' (no owner/repo mapping; set FLOW_INSTALL_OWNER or pass owner/repo)",
            name
        );
    }

    match install_with_flox(opts) {
        Ok(()) => Ok(()),
        Err(err) => {
            errors.push(format!("flox: {err}"));
            bail!(
                "install failed after trying auto backends:\n- {}",
                errors.join("\n- ")
            );
        }
    }
}

pub fn run_index(opts: InstallIndexOpts) -> Result<()> {
    let flox_bin = resolve_flox_bin()?;
    let Some(config) = typesense_config_with_overrides(&opts) else {
        bail!("Typesense config missing (set FLOW_TYPESENSE_URL or pass --url)");
    };

    let queries = load_index_queries(opts.query, opts.queries)?;
    if queries.is_empty() {
        bail!("no queries provided");
    }

    let mut all_entries: HashMap<String, FloxDisplayEntry> = HashMap::new();
    for query in queries {
        let results = flox_search_with_aliases(&flox_bin, &query)?;
        for entry in results {
            all_entries.entry(entry.pkg_path.clone()).or_insert(entry);
        }
    }

    if all_entries.is_empty() {
        println!("No results to index.");
        return Ok(());
    }

    if opts.dry_run {
        println!("Would index {} packages into Typesense.", all_entries.len());
        return Ok(());
    }

    typesense_ensure_collection(&config)?;
    typesense_import(&config, all_entries.values().cloned().collect())?;
    println!("Indexed {} packages into Typesense.", all_entries.len());
    Ok(())
}

fn registry_configured(_opts: &InstallOpts) -> bool {
    // Registry is always available — defaults to https://myflow.sh
    true
}

fn install_with_flox(opts: &InstallOpts) -> Result<()> {
    let name = opts.name.as_deref().unwrap_or("").trim();
    if name.is_empty() {
        bail!("package name is required");
    }

    let install_root = tool_root()?;
    let flox_pkg = resolve_flox_pkg_name(name);
    let spec = FloxInstallSpec {
        pkg_path: flox_pkg.to_string(),
        pkg_group: Some("tools".to_string()),
        version: opts.version.clone(),
        systems: None,
        priority: None,
    };

    ensure_flox_tools_env(&install_root, &[(flox_pkg.to_string(), spec)])?;

    let bin_name = opts.bin.clone().unwrap_or_else(|| name.to_string());
    let bin_dir = opts.bin_dir.clone().unwrap_or_else(default_bin_dir);
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("failed to create {}", bin_dir.display()))?;

    let shim_path = bin_dir.join(&bin_name);
    if shim_path.exists() && !opts.force {
        if shim_matches(&shim_path, &install_root, &bin_name).unwrap_or(false) {
            println!("{} already installed via flox.", bin_name);
            return Ok(());
        }
        if prompt_overwrite(&shim_path)? {
            // continue and overwrite
        } else {
            bail!(
                "{} already exists (use --force to overwrite or --bin to install under a different name)",
                shim_path.display()
            );
        }
    }

    write_flox_shim(&shim_path, &install_root, &bin_name)?;

    if flox_pkg != name {
        println!(
            "Installed {} (flox package {}) via flox (shim at {})",
            name,
            flox_pkg,
            shim_path.display()
        );
    } else {
        println!(
            "Installed {} via flox (shim at {})",
            name,
            shim_path.display()
        );
    }
    if !path_in_env(&bin_dir) {
        println!("Add {} to PATH to use it everywhere.", bin_dir.display());
    }
    Ok(())
}

fn install_with_parm(opts: &InstallOpts) -> Result<()> {
    let name = opts.name.as_deref().unwrap_or("").trim();
    if name.is_empty() {
        bail!("package name is required");
    }

    if !opts.bin.is_none() {
        // Parm determines which executables exist inside the release asset.
        // We keep Flow's `--bin` flag for other backends, but it doesn't map cleanly.
        eprintln!("Note: --bin is ignored for --backend parm");
    }
    if opts.force {
        eprintln!("Note: --force is ignored for --backend parm");
    }

    let bin_dir = opts.bin_dir.clone().unwrap_or_else(default_bin_dir);
    fs::create_dir_all(&bin_dir)
        .with_context(|| format!("failed to create {}", bin_dir.display()))?;

    let owner_repo = resolve_owner_repo(name)?;
    let owner_repo = match opts
        .version
        .as_deref()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
    {
        Some(version) => format!("{}@{}", owner_repo, version),
        None => owner_repo,
    };

    let parm_bin = which::which("parm").context(
        "parm not found on PATH. Install it first (macOS/Linux):\n  curl -fsSL https://raw.githubusercontent.com/yhoundz/parm/master/scripts/install.sh | sh",
    )?;

    // Configure parm to symlink into the same directory Flow uses for tools.
    // This makes installs predictable and avoids relying on parm defaults.
    let config_status = Command::new(&parm_bin)
        .args([
            "config",
            "set",
            &format!("parm_bin_path={}", bin_dir.display()),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run parm config set")?;
    if !config_status.success() {
        bail!("parm config set failed");
    }

    let mut cmd = Command::new(&parm_bin);
    cmd.args(["install", &owner_repo]);
    if opts.no_verify {
        cmd.arg("--no-verify");
    }

    if let Some(token) = resolve_github_token()? {
        cmd.env("PARM_GITHUB_TOKEN", token);
    }

    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run parm install")?;
    if !status.success() {
        bail!("parm install failed");
    }

    if !path_in_env(&bin_dir) {
        println!("Add {} to PATH to use it everywhere.", bin_dir.display());
    }
    Ok(())
}

fn resolve_owner_repo(raw: &str) -> Result<String> {
    if raw.contains('/') {
        return Ok(raw.to_string());
    }

    if let Some(mapped) = known_owner_repo(raw) {
        return Ok(mapped.to_string());
    }

    // Prefer explicit env var; fall back to Flow personal env store.
    let owner = resolve_install_owner();

    let Some(owner) = owner else {
        bail!(
            "package name '{}' is missing owner (expected owner/repo).\nSet FLOW_INSTALL_OWNER (env or Flow personal env store), use a known alias (flow/rise), or pass owner/repo directly.",
            raw
        );
    };

    Ok(format!("{}/{}", owner, raw))
}

fn should_try_parm(opts: &InstallOpts) -> bool {
    let Some(name) = opts.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) else {
        return false;
    };
    name.contains('/') || known_owner_repo(name).is_some() || resolve_install_owner().is_some()
}

fn known_owner_repo(name: &str) -> Option<&'static str> {
    match name {
        "f" | "flow" | "lin" => Some("nikivdev/flow"),
        "rise" => Some("nikivdev/rise"),
        _ => None,
    }
}

fn resolve_install_owner() -> Option<String> {
    std::env::var("FLOW_INSTALL_OWNER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            crate::env::get_personal_env_var("FLOW_INSTALL_OWNER")
                .ok()
                .flatten()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

fn resolve_github_token() -> Result<Option<String>> {
    for key in ["PARM_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Ok(Some(trimmed.to_string()));
            }
        }
    }

    for key in [
        "PARM_GITHUB_TOKEN",
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "FLOW_GITHUB_TOKEN",
    ] {
        if let Ok(Some(value)) = crate::env::get_personal_env_var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Ok(Some(trimmed.to_string()));
            }
        }
    }

    Ok(None)
}

fn ensure_flox_tools_env(root: &Path, packages: &[(String, FloxInstallSpec)]) -> Result<()> {
    let flox_bin = resolve_flox_bin()?;

    if flox_env_ok(&flox_bin, root).is_err() {
        let flox_dir = root.join(".flox");
        if flox_dir.exists() {
            fs::remove_dir_all(&flox_dir)
                .with_context(|| format!("failed to remove {}", flox_dir.display()))?;
        }
    }

    let flox_dir = root.join(".flox");
    if !flox_dir.exists() {
        flox_run(
            &flox_bin,
            &[
                "init".to_string(),
                "--bare".to_string(),
                "-d".to_string(),
                root.display().to_string(),
            ],
        )?;
    }

    for (name, spec) in packages {
        let pkg = match spec.version.as_deref() {
            Some(version) if !version.trim().is_empty() => format!("{name}@{version}"),
            _ => name.to_string(),
        };
        flox_run(
            &flox_bin,
            &[
                "install".to_string(),
                "-d".to_string(),
                root.display().to_string(),
                pkg,
            ],
        )?;
    }

    if let Err(err) = flox_env_ok(&flox_bin, root) {
        bail!("flox env still invalid after reset: {err}");
    }
    Ok(())
}

fn resolve_flox_bin() -> Result<PathBuf> {
    if let Ok(path) = env::var("FLOX_BIN") {
        let bin = PathBuf::from(path);
        if bin.exists() {
            return Ok(bin);
        }
    }
    if let Ok(path) = which::which("flox") {
        return Ok(path);
    }
    bail!("flox not found on PATH")
}

fn flox_run(flox_bin: &Path, args: &[String]) -> Result<()> {
    let status = std::process::Command::new(flox_bin)
        .args(args)
        .status()
        .with_context(|| format!("failed to run flox {}", args.join(" ")))?;
    if !status.success() {
        bail!("flox {} failed", args.join(" "));
    }
    Ok(())
}

fn flox_env_ok(flox_bin: &Path, root: &Path) -> Result<()> {
    let output = std::process::Command::new(flox_bin)
        .arg("activate")
        .arg("-d")
        .arg(root)
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg("true")
        .output()
        .context("failed to run flox activate")?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("{}", stderr.trim());
}

fn resolve_flox_pkg_name(name: &str) -> &str {
    match name {
        "jj" => "jujutsu",
        _ => name,
    }
}

fn prompt_flox_package() -> Result<String> {
    if !io::stdin().is_terminal() {
        bail!("package name is required (interactive search needs a TTY)");
    }

    if which::which("fzf").is_err() {
        return prompt_line("Package name", None);
    }

    let query = prompt_line("Search flox for", None)?;
    let query = query.trim();
    if query.is_empty() {
        bail!("package name is required");
    }

    let entries = match typesense_config() {
        Some(config) => match typesense_search(&config, query) {
            Ok(entries) if !entries.is_empty() => entries,
            Ok(_) => {
                let flox_bin = resolve_flox_bin()?;
                flox_search_with_aliases(&flox_bin, query)?
            }
            Err(err) => {
                eprintln!("WARN typesense search failed: {err}");
                let flox_bin = resolve_flox_bin()?;
                flox_search_with_aliases(&flox_bin, query)?
            }
        },
        None => {
            let flox_bin = resolve_flox_bin()?;
            flox_search_with_aliases(&flox_bin, query)?
        }
    };
    if entries.is_empty() {
        bail!("no flox packages found for \"{}\"", query);
    }

    let mut input = String::new();
    for entry in &entries {
        let version = entry.version.as_deref().unwrap_or("-");
        let desc = entry
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .unwrap_or("No description");
        let alias_note = entry
            .alias
            .as_deref()
            .map(|alias| format!(" (alias for {})", alias))
            .unwrap_or_default();
        input.push_str(&format!(
            "{}\t{}\t{}{}\n",
            entry.pkg_path, version, desc, alias_note
        ));
    }

    let mut child = std::process::Command::new("fzf")
        .args([
            "--height=50%",
            "--reverse",
            "--delimiter=\t",
            "--with-nth=1,3",
            "--prompt=flox> ",
            "--preview=echo Version: {2}\\n\\n{3}",
            "--preview-window=right,60%,wrap",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn fzf")?;

    child
        .stdin
        .as_mut()
        .context("failed to open fzf stdin")?
        .write_all(input.as_bytes())?;

    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("no package selected");
    }

    let selection = String::from_utf8(output.stdout).context("fzf output was not valid UTF-8")?;
    let selected = selection.trim().split('\t').next().unwrap_or("");
    if selected.is_empty() {
        bail!("no package selected");
    }
    Ok(selected.to_string())
}

#[derive(Clone, Debug, Deserialize)]
struct FloxSearchEntry {
    #[serde(rename = "pkg_path")]
    pkg_path: String,
    description: Option<String>,
    version: Option<String>,
}

#[derive(Clone, Debug)]
struct FloxDisplayEntry {
    pkg_path: String,
    description: Option<String>,
    version: Option<String>,
    alias: Option<String>,
}

#[derive(Clone, Debug)]
struct TypesenseConfig {
    url: String,
    api_key: String,
    collection: String,
}

#[derive(Debug, Deserialize)]
struct TypesenseSearchResponse {
    hits: Vec<TypesenseHit>,
}

#[derive(Debug, Deserialize)]
struct TypesenseHit {
    document: TypesenseDoc,
}

#[derive(Debug, Deserialize)]
struct TypesenseDoc {
    #[serde(rename = "pkg_path")]
    pkg_path: String,
    description: Option<String>,
    version: Option<String>,
}

fn typesense_config() -> Option<TypesenseConfig> {
    let url = env::var("FLOW_TYPESENSE_URL").ok()?;
    let api_key = env::var("FLOW_TYPESENSE_API_KEY").unwrap_or_default();
    let collection =
        env::var("FLOW_TYPESENSE_COLLECTION").unwrap_or_else(|_| "flox-packages".to_string());
    Some(TypesenseConfig {
        url,
        api_key,
        collection,
    })
}

fn typesense_config_with_overrides(opts: &InstallIndexOpts) -> Option<TypesenseConfig> {
    let url = opts
        .url
        .clone()
        .or_else(|| env::var("FLOW_TYPESENSE_URL").ok())?;
    let api_key = opts
        .api_key
        .clone()
        .or_else(|| env::var("FLOW_TYPESENSE_API_KEY").ok())
        .unwrap_or_default();
    let collection = opts.collection.clone();
    Some(TypesenseConfig {
        url,
        api_key,
        collection,
    })
}

fn typesense_search(config: &TypesenseConfig, query: &str) -> Result<Vec<FloxDisplayEntry>> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let url = format!(
        "{}/collections/{}/documents/search",
        config.url.trim_end_matches('/'),
        config.collection
    );
    let payload = serde_json::json!({
        "q": query,
        "query_by": "pkg_path,description",
        "per_page": 200,
    });
    let mut request = client.post(url).json(&payload);
    if !config.api_key.is_empty() {
        request = request.header("X-TYPESENSE-API-KEY", &config.api_key);
    }
    let response = request.send().context("failed to query typesense")?;
    if !response.status().is_success() {
        bail!("typesense returned {}", response.status());
    }
    let body: TypesenseSearchResponse = response
        .json()
        .context("failed to parse typesense response")?;
    let mut entries = Vec::new();
    for hit in body.hits {
        entries.push(FloxDisplayEntry {
            pkg_path: hit.document.pkg_path,
            description: hit.document.description,
            version: hit.document.version,
            alias: None,
        });
    }
    Ok(entries)
}

fn typesense_ensure_collection(config: &TypesenseConfig) -> Result<()> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let base = config.url.trim_end_matches('/');
    let get_url = format!("{}/collections/{}", base, config.collection);
    let mut request = client.get(&get_url);
    if !config.api_key.is_empty() {
        request = request.header("X-TYPESENSE-API-KEY", &config.api_key);
    }
    let resp = request
        .send()
        .context("failed to check typesense collection")?;
    if resp.status().is_success() {
        return Ok(());
    }
    if resp.status().as_u16() != 404 {
        bail!("typesense collection check failed ({})", resp.status());
    }

    let create_url = format!("{}/collections", base);
    let schema = serde_json::json!({
        "name": config.collection,
        "fields": [
            { "name": "id", "type": "string" },
            { "name": "pkg_path", "type": "string" },
            { "name": "description", "type": "string", "optional": true },
            { "name": "version", "type": "string", "optional": true }
        ],
        "default_sorting_field": "pkg_path"
    });
    let mut create_req = client.post(&create_url).json(&schema);
    if !config.api_key.is_empty() {
        create_req = create_req.header("X-TYPESENSE-API-KEY", &config.api_key);
    }
    let resp = create_req
        .send()
        .context("failed to create typesense collection")?;
    if !resp.status().is_success() {
        bail!("typesense collection create failed ({})", resp.status());
    }
    Ok(())
}

fn typesense_import(config: &TypesenseConfig, entries: Vec<FloxDisplayEntry>) -> Result<()> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let base = config.url.trim_end_matches('/');
    let url = format!(
        "{}/collections/{}/documents/import?action=upsert",
        base, config.collection
    );
    let mut body = String::new();
    for entry in entries {
        let doc = serde_json::json!({
            "id": entry.pkg_path,
            "pkg_path": entry.pkg_path,
            "description": entry.description,
            "version": entry.version
        });
        body.push_str(&doc.to_string());
        body.push('\n');
    }
    let mut request = client.post(&url).body(body);
    if !config.api_key.is_empty() {
        request = request.header("X-TYPESENSE-API-KEY", &config.api_key);
    }
    let resp = request.send().context("failed to import into typesense")?;
    if !resp.status().is_success() {
        bail!("typesense import failed ({})", resp.status());
    }
    Ok(())
}

fn load_index_queries(query: Option<String>, path: Option<PathBuf>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if let Some(path) = path {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            out.push(trimmed.to_string());
        }
    }
    if let Some(query) = query {
        let trimmed = query.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    if out.is_empty() && io::stdin().is_terminal() {
        let input = prompt_line("Search term to index", None)?;
        let trimmed = input.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

fn flox_search_with_aliases(flox_bin: &Path, query: &str) -> Result<Vec<FloxDisplayEntry>> {
    let mut seen = HashMap::<String, FloxDisplayEntry>::new();

    let mut queries = vec![query.to_string()];
    if let Some(extra) = flox_query_aliases(query) {
        queries.extend(extra.iter().map(|q| q.to_string()));
    }

    for q in queries {
        let results = flox_search(flox_bin, &q)?;
        for result in results {
            let entry = FloxDisplayEntry {
                pkg_path: result.pkg_path.clone(),
                description: result.description.clone(),
                version: result.version.clone(),
                alias: None,
            };
            seen.entry(result.pkg_path).or_insert(entry);
        }
    }

    if let Some(alias_targets) = flox_query_aliases(query) {
        for alias_target in alias_targets {
            let alias_results = flox_search(flox_bin, alias_target)?;
            let picked = alias_results
                .iter()
                .find(|entry| entry.pkg_path == *alias_target)
                .or_else(|| alias_results.first());
            if let Some(result) = picked {
                seen.insert(
                    result.pkg_path.clone(),
                    FloxDisplayEntry {
                        pkg_path: result.pkg_path.clone(),
                        description: result.description.clone(),
                        version: result.version.clone(),
                        alias: Some(query.to_string()),
                    },
                );
            }
        }
    }

    let mut entries: Vec<_> = seen.into_values().collect();
    entries.sort_by(|a, b| flox_entry_rank(a, query).cmp(&flox_entry_rank(b, query)));
    Ok(entries)
}

fn flox_query_aliases(query: &str) -> Option<&'static [&'static str]> {
    match query {
        "jj" => Some(&["jujutsu"]),
        _ => None,
    }
}

fn flox_entry_rank(entry: &FloxDisplayEntry, query: &str) -> (u8, String) {
    if entry.pkg_path == query {
        return (0, entry.pkg_path.clone());
    }
    if entry.alias.is_some() {
        return (1, entry.pkg_path.clone());
    }
    if entry
        .description
        .as_deref()
        .map(|d| d.to_ascii_lowercase().contains(&query.to_ascii_lowercase()))
        .unwrap_or(false)
    {
        return (2, entry.pkg_path.clone());
    }
    (3, entry.pkg_path.clone())
}

fn flox_search(flox_bin: &Path, query: &str) -> Result<Vec<FloxSearchEntry>> {
    let output = std::process::Command::new(flox_bin)
        .arg("search")
        .arg("--json")
        .arg("-a")
        .arg(query)
        .output()
        .with_context(|| format!("failed to run flox search {}", query))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("flox search failed: {}", stderr.trim());
    }
    let stdout =
        String::from_utf8(output.stdout).context("flox search output was not valid UTF-8")?;
    let entries: Vec<FloxSearchEntry> = serde_json::from_str(&stdout)
        .with_context(|| format!("failed to parse flox search output for {}", query))?;
    Ok(entries)
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

fn tool_root() -> Result<PathBuf> {
    let home = dirs::home_dir().context("failed to resolve home directory")?;
    Ok(home.join(".config").join("flow").join("tools"))
}

fn write_flox_shim(dest: &Path, env_root: &Path, bin: &str) -> Result<()> {
    let script = format!(
        "#!/bin/sh\nexec flox activate -d \"{}\" -- \"{}\" \"$@\"\n",
        env_root.display(),
        bin
    );
    fs::write(dest, script).with_context(|| format!("failed to write {}", dest.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(dest)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(dest, perms)?;
    }
    Ok(())
}

fn shim_matches(dest: &Path, env_root: &Path, bin: &str) -> Result<bool> {
    let content =
        fs::read_to_string(dest).with_context(|| format!("failed to read {}", dest.display()))?;
    let expected = format!(
        "#!/bin/sh\nexec flox activate -d \"{}\" -- \"{}\" \"$@\"\n",
        env_root.display(),
        bin
    );
    Ok(content == expected)
}

fn prompt_overwrite(path: &Path) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("{} already exists. Overwrite? [y/N]: ", path.display());
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn default_bin_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let local = home.join(".local").join("bin");
    if local.exists() {
        return local;
    }
    let bin = home.join("bin");
    if bin.exists() {
        return bin;
    }
    local
}

fn path_in_env(bin_dir: &Path) -> bool {
    let Ok(path) = env::var("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|entry| entry == bin_dir)
}
