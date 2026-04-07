use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::cli::{CliToolAction, CliToolCommand};
use crate::config;

const MANIFEST_NAME: &str = "flow-tool.toml";
const LINK_RECORD_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
pub struct ExternalCliManifest {
    pub version: u32,
    pub id: String,
    pub language: String,
    pub binary_name: String,
    pub description: Option<String>,
    pub exec: ExternalCliExec,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExternalCliExec {
    pub run: Vec<String>,
    pub build: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExternalCliLinkRecord {
    version: u32,
    id: String,
    source_root: PathBuf,
    manifest_path: PathBuf,
    installed_at: String,
    description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalCliResolutionKind {
    InstalledLink,
    DevRoot,
}

impl ExternalCliResolutionKind {
    fn label(self) -> &'static str {
        match self {
            Self::InstalledLink => "installed-link",
            Self::DevRoot => "dev-root",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedExternalCliTool {
    pub source_root: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: ExternalCliManifest,
    pub resolution: ExternalCliResolutionKind,
    pub registration_path: Option<PathBuf>,
}

pub fn run_command(cmd: CliToolCommand) -> Result<()> {
    match cmd.action {
        Some(CliToolAction::List) => print_cli_list(),
        Some(CliToolAction::Which { id }) => print_cli_which(&id),
        Some(CliToolAction::Doctor { id }) => print_cli_doctor(&id),
        None => {
            if let Some(id) = cmd
                .id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                run_external_cli(id, cmd.args.iter().map(String::as_str))
            } else {
                print_cli_list()
            }
        }
    }
}

pub fn resolve_external_cli_tool(id: &str) -> Result<ResolvedExternalCliTool> {
    if let Some(installed) = resolve_installed_external_cli_tool(id)? {
        return Ok(installed);
    }

    let roots = default_external_cli_roots()?;
    resolve_external_cli_tool_in_roots(id, &roots)
}

pub fn list_external_cli_tools() -> Result<Vec<ResolvedExternalCliTool>> {
    let mut tools = BTreeMap::new();

    for (record, record_path) in load_link_records()? {
        let tool = resolved_from_link_record(&record, Some(record_path))?;
        tools.insert(tool.manifest.id.clone(), tool);
    }

    for root in default_external_cli_roots()? {
        for candidate in candidate_manifest_paths(&root) {
            let manifest = read_manifest(&candidate)?;
            if tools.contains_key(&manifest.id) {
                continue;
            }
            let tool = resolved_from_manifest(
                candidate,
                manifest,
                ExternalCliResolutionKind::DevRoot,
                None,
            )?;
            tools.insert(tool.manifest.id.clone(), tool);
        }
    }

    Ok(tools.into_values().collect())
}

pub fn install_external_cli_link(path: &Path, force: bool) -> Result<ResolvedExternalCliTool> {
    let (source_root, manifest_path) = resolve_install_source_paths(path)?;
    let source_root = fs::canonicalize(&source_root)
        .with_context(|| format!("failed to resolve {}", source_root.display()))?;
    let manifest_path = fs::canonicalize(&manifest_path)
        .with_context(|| format!("failed to resolve {}", manifest_path.display()))?;
    let manifest = read_manifest(&manifest_path)?;

    let record = ExternalCliLinkRecord {
        version: LINK_RECORD_VERSION,
        id: manifest.id.clone(),
        source_root,
        manifest_path,
        installed_at: Utc::now().to_rfc3339(),
        description: manifest.description.clone(),
    };

    let links_dir = ensure_link_records_dir()?;
    let record_path = links_dir.join(format!("{}.toml", record.id));

    if record_path.is_file() {
        let existing = read_link_record(&record_path)?;
        if !same_link_target(&existing, &record) && !force {
            bail!(
                "external CLI {} is already linked to {} (use --force to replace it)",
                record.id,
                existing.source_root.display()
            );
        }
    }

    let content = toml::to_string_pretty(&record)
        .with_context(|| format!("failed to serialize {}", record.id))?;
    fs::write(&record_path, content)
        .with_context(|| format!("failed to write {}", record_path.display()))?;

    resolved_from_link_record(&record, Some(record_path))
}

pub fn command_for_external_cli<I, S>(
    id: &str,
    args: I,
) -> Result<(ResolvedExternalCliTool, Command)>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let tool = resolve_external_cli_tool(id)?;
    let command = tool.command(args)?;
    Ok((tool, command))
}

pub fn run_external_cli<I, S>(id: &str, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let (tool, mut command) = command_for_external_cli(id, args)?;
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = command.status().with_context(|| {
        format!(
            "failed to launch external CLI {} from {}",
            id,
            tool.manifest_path.display()
        )
    })?;

    ensure_success(id, status)
}

impl ResolvedExternalCliTool {
    pub fn command<I, S>(&self, args: I) -> Result<Command>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if self.manifest.exec.run.is_empty() {
            bail!(
                "{} has no exec.run argv in {}",
                self.manifest.id,
                self.manifest_path.display()
            );
        }

        let mut argv = self.manifest.exec.run.iter();
        let Some(program) = argv.next() else {
            bail!(
                "{} has no exec.run program in {}",
                self.manifest.id,
                self.manifest_path.display()
            );
        };

        let mut command = Command::new(program);
        command.current_dir(&self.source_root);
        command.args(argv);
        for arg in args {
            command.arg(arg.as_ref());
        }

        if let Some(env_map) = &self.manifest.exec.env {
            command.envs(env_map);
        }

        Ok(command)
    }
}

fn print_cli_list() -> Result<()> {
    let tools = list_external_cli_tools()?;
    if tools.is_empty() {
        println!("No external CLIs found.");
        return Ok(());
    }

    for tool in tools {
        println!(
            "{}\t{}\t{}",
            tool.manifest.id,
            tool.resolution.label(),
            tool.source_root.display()
        );
    }

    Ok(())
}

fn print_cli_which(id: &str) -> Result<()> {
    let tool = resolve_external_cli_tool(id)?;
    println!("{}", tool.source_root.display());
    Ok(())
}

fn print_cli_doctor(id: &str) -> Result<()> {
    let tool = resolve_external_cli_tool(id)?;
    println!("id: {}", tool.manifest.id);
    println!("resolution: {}", tool.resolution.label());
    println!("source_root: {}", tool.source_root.display());
    println!("manifest_path: {}", tool.manifest_path.display());
    if let Some(path) = &tool.registration_path {
        println!("registration_path: {}", path.display());
    }
    println!("language: {}", tool.manifest.language);
    println!("binary_name: {}", tool.manifest.binary_name);
    if let Some(description) = &tool.manifest.description {
        println!("description: {}", description);
    }
    println!("run: {}", render_argv(&tool.manifest.exec.run));
    if let Some(build) = &tool.manifest.exec.build {
        println!("build: {}", render_argv(build));
    }
    if let Some(env_map) = &tool.manifest.exec.env {
        if env_map.is_empty() {
            println!("env: none");
        } else {
            let mut keys = env_map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            println!("env: {}", keys.join(", "));
        }
    } else {
        println!("env: none");
    }
    Ok(())
}

fn ensure_success(id: &str, status: ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!(
            "external CLI {} exited unsuccessfully with status {}",
            id,
            status
        )
    }
}

fn render_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg.chars().any(char::is_whitespace) {
                format!("{arg:?}")
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn same_link_target(left: &ExternalCliLinkRecord, right: &ExternalCliLinkRecord) -> bool {
    left.source_root == right.source_root && left.manifest_path == right.manifest_path
}

fn resolve_installed_external_cli_tool(id: &str) -> Result<Option<ResolvedExternalCliTool>> {
    let record_path = link_record_path(id);
    if !record_path.is_file() {
        return Ok(None);
    }

    let record = read_link_record(&record_path)?;
    Ok(Some(resolved_from_link_record(&record, Some(record_path))?))
}

fn load_link_records() -> Result<Vec<(ExternalCliLinkRecord, PathBuf)>> {
    let links_dir = link_records_dir();
    if !links_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut records = Vec::new();
    for path in candidate_link_record_paths(&links_dir) {
        let record = read_link_record(&path)?;
        records.push((record, path));
    }
    Ok(records)
}

fn default_external_cli_roots() -> Result<Vec<PathBuf>> {
    let home =
        dirs::home_dir().context("could not resolve home directory for external CLI roots")?;
    Ok(vec![
        home.join("code/lang/go/cli"),
        home.join("code/lang/rust/cli"),
    ])
}

fn resolve_external_cli_tool_in_roots(
    id: &str,
    roots: &[PathBuf],
) -> Result<ResolvedExternalCliTool> {
    let mut matches = Vec::new();

    for root in roots {
        for candidate in candidate_manifest_paths(root) {
            let manifest = read_manifest(&candidate)?;
            if manifest.id == id {
                matches.push(resolved_from_manifest(
                    candidate,
                    manifest,
                    ExternalCliResolutionKind::DevRoot,
                    None,
                )?);
            }
        }
    }

    match matches.len() {
        0 => bail!(
            "external CLI tool {} not found under {}",
            id,
            roots
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        1 => Ok(matches.remove(0)),
        _ => {
            let locations = matches
                .iter()
                .map(|tool| tool.manifest_path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "multiple external CLI manifests matched {}: {}",
                id,
                locations
            )
        }
    }
}

fn resolved_from_link_record(
    record: &ExternalCliLinkRecord,
    registration_path: Option<PathBuf>,
) -> Result<ResolvedExternalCliTool> {
    let manifest = read_manifest(&record.manifest_path)?;
    if manifest.id != record.id {
        bail!(
            "external CLI link {} points to manifest with mismatched id {}",
            record.id,
            manifest.id
        );
    }
    resolved_from_manifest(
        record.manifest_path.clone(),
        manifest,
        ExternalCliResolutionKind::InstalledLink,
        registration_path,
    )
}

fn resolved_from_manifest(
    manifest_path: PathBuf,
    manifest: ExternalCliManifest,
    resolution: ExternalCliResolutionKind,
    registration_path: Option<PathBuf>,
) -> Result<ResolvedExternalCliTool> {
    let source_root = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    Ok(ResolvedExternalCliTool {
        source_root,
        manifest_path,
        manifest,
        resolution,
        registration_path,
    })
}

fn resolve_install_source_paths(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let meta = fs::metadata(path)
        .with_context(|| format!("failed to read external CLI source {}", path.display()))?;
    if meta.is_dir() {
        let manifest_path = path.join(MANIFEST_NAME);
        if !manifest_path.is_file() {
            bail!("{} does not contain {}", path.display(), MANIFEST_NAME);
        }
        return Ok((path.to_path_buf(), manifest_path));
    }

    if meta.is_file() && path.file_name().and_then(|name| name.to_str()) == Some(MANIFEST_NAME) {
        let source_root = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        return Ok((source_root, path.to_path_buf()));
    }

    bail!(
        "{} must be an external CLI source directory or {} file",
        path.display(),
        MANIFEST_NAME
    )
}

fn link_record_path(id: &str) -> PathBuf {
    link_records_dir().join(format!("{}.toml", id))
}

fn link_records_dir() -> PathBuf {
    config::global_config_dir().join("cli").join("links")
}

fn ensure_link_records_dir() -> Result<PathBuf> {
    let dir = link_records_dir();
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir)
}

fn candidate_link_record_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return paths;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("toml") {
            continue;
        }
        if path.is_file() {
            paths.push(path);
        }
    }

    paths
}

fn candidate_manifest_paths(root: &Path) -> Vec<PathBuf> {
    let mut manifests = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return manifests;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest = path.join(MANIFEST_NAME);
        if manifest.is_file() {
            manifests.push(manifest);
        }
    }

    manifests
}

fn read_link_record(path: &Path) -> Result<ExternalCliLinkRecord> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let record: ExternalCliLinkRecord =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    if record.version != LINK_RECORD_VERSION {
        bail!(
            "unsupported external CLI link record version {} in {}",
            record.version,
            path.display()
        );
    }
    Ok(record)
}

fn read_manifest(path: &Path) -> Result<ExternalCliManifest> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let manifest: ExternalCliManifest =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    if manifest.version != 1 {
        bail!(
            "unsupported external CLI manifest version {} in {}",
            manifest.version,
            path.display()
        );
    }
    if manifest.exec.run.is_empty() {
        bail!("missing exec.run in {}", path.display());
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tool(root: &Path, id: &str) -> PathBuf {
        let tool_root = root.join(id);
        fs::create_dir_all(&tool_root).expect("create tool root");
        fs::write(
            tool_root.join(MANIFEST_NAME),
            format!(
                r#"
version = 1
id = "{id}"
language = "go"
binary_name = "{id}"

[exec]
run = ["go", "run", "."]
"#
            ),
        )
        .expect("write manifest");
        tool_root
    }

    #[test]
    fn resolves_manifest_from_roots() {
        let temp = tempfile::tempdir().expect("tempdir");
        let go_root = temp.path().join("go");
        let tool_root = write_tool(&go_root, "codex-session-browser");

        let resolved =
            resolve_external_cli_tool_in_roots("codex-session-browser", &[go_root.clone()])
                .expect("resolve tool");

        assert_eq!(resolved.manifest.id, "codex-session-browser");
        assert_eq!(resolved.source_root, tool_root);
        assert_eq!(resolved.resolution, ExternalCliResolutionKind::DevRoot);
        assert_eq!(resolved.manifest.exec.run[0], "go");
    }

    #[test]
    fn install_link_writes_record_and_resolves() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool_root = write_tool(temp.path(), "codex-session-browser");
        let links_dir = temp.path().join("links");
        fs::create_dir_all(&links_dir).expect("create links dir");

        let manifest_path = tool_root.join(MANIFEST_NAME);
        let record = ExternalCliLinkRecord {
            version: LINK_RECORD_VERSION,
            id: "codex-session-browser".to_string(),
            source_root: tool_root.clone(),
            manifest_path: manifest_path.clone(),
            installed_at: Utc::now().to_rfc3339(),
            description: None,
        };
        let record_path = links_dir.join("codex-session-browser.toml");
        let content = toml::to_string_pretty(&record).expect("serialize record");
        fs::write(&record_path, content).expect("write record");

        let loaded = read_link_record(&record_path).expect("read record");
        let resolved =
            resolved_from_link_record(&loaded, Some(record_path.clone())).expect("resolve record");

        assert_eq!(resolved.manifest.id, "codex-session-browser");
        assert_eq!(resolved.source_root, tool_root);
        assert_eq!(resolved.manifest_path, manifest_path);
        assert_eq!(resolved.registration_path, Some(record_path));
        assert_eq!(
            resolved.resolution,
            ExternalCliResolutionKind::InstalledLink
        );
    }
}
