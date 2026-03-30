use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use which::which;

use crate::cli::{ConfigAction, ConfigCommand};
use crate::config::{self, TsFlowConfig};

const TS_CONFIG_LOADER: &str = r#"#!/usr/bin/env node
import { pathToFileURL } from "node:url";

async function main() {
  const target = process.argv[2];
  if (!target) {
    console.error("missing config path");
    process.exit(1);
  }

  const mod = await import(pathToFileURL(target).href);
  let cfg = mod.default ?? mod.config ?? mod;
  if (cfg && typeof cfg.then === "function") {
    cfg = await cfg;
  }
  if (cfg === undefined || cfg === null) {
    console.error("config module exported nothing");
    process.exit(1);
  }
  console.log(JSON.stringify(cfg));
}

main().catch((err) => {
  console.error(err?.stack || err?.message || err);
  process.exit(1);
});
"#;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RootTsConfig {
    #[serde(default)]
    pub flow: Option<TsFlowConfig>,
    #[serde(default)]
    pub lin: Option<Value>,
    #[serde(default)]
    pub ai: Option<Value>,
    #[serde(default)]
    pub hive: Option<Value>,
    #[serde(default)]
    pub zerg: Option<Value>,
    #[serde(default)]
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct LegacyLinSourceConfig {
    #[serde(default)]
    flow: Option<TsFlowConfig>,
    #[serde(default)]
    ai: Option<Value>,
    #[serde(default)]
    hive: Option<Value>,
    #[serde(default)]
    intents: Option<Value>,
    #[serde(default)]
    services: Option<Value>,
    #[serde(default)]
    mac: Option<Value>,
    #[serde(default)]
    servers: Option<Value>,
    #[serde(default, rename = "globalTools", alias = "global_tools")]
    global_tools: Option<Value>,
    #[serde(default)]
    watchers: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct FlowExtensionConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub flow: Option<TsFlowConfig>,
    #[serde(default)]
    pub lin: Option<Value>,
    #[serde(default)]
    pub ai: Option<Value>,
    #[serde(default)]
    pub hive: Option<Value>,
    #[serde(default)]
    pub zerg: Option<Value>,
    #[serde(default, rename = "generatedFiles", alias = "generated_files")]
    pub generated_files: Vec<ExtensionGeneratedFile>,
    #[serde(default)]
    pub doctor: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ExtensionGeneratedFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct EffectiveFlowConfig {
    #[serde(default)]
    pub flow: Option<TsFlowConfig>,
    #[serde(default)]
    pub lin: Option<Value>,
    #[serde(default)]
    pub ai: Option<Value>,
    #[serde(default)]
    pub hive: Option<Value>,
    #[serde(default)]
    pub zerg: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResolvedExtension {
    pub name: String,
    pub kind: String,
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GeneratedArtifact {
    pub id: String,
    pub generated_path: String,
    pub apply_path: String,
    pub source: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConfigSnapshot {
    pub built_at_unix: u64,
    pub root_config_path: String,
    pub generated_dir: String,
    pub enabled_extensions: Vec<String>,
    pub resolved_extensions: Vec<ResolvedExtension>,
    pub warnings: Vec<String>,
    pub effective: EffectiveFlowConfig,
    pub generated_artifacts: Vec<GeneratedArtifact>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExtensionStatus {
    pub name: String,
    pub enabled: bool,
    pub kind: String,
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DoctorReport {
    pub root_config_path: String,
    pub root_config_exists: bool,
    pub generated_snapshot_path: String,
    pub generated_snapshot_exists: bool,
    pub ts_runner: Option<String>,
    pub extension_dirs: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct TsRunner {
    display: String,
    bin: String,
    args: Vec<String>,
}

const FLOW_CONFIG_RUNTIME_HELPER: &str = r#"export type FlowGeneratedFile = {
  path: string
  content: string
}

export type FlowExtensionResult = {
  flow?: Record<string, unknown>
  lin?: Record<string, unknown>
  ai?: Record<string, unknown>
  hive?: Record<string, unknown>
  zerg?: Record<string, unknown>
  generatedFiles?: FlowGeneratedFile[]
  doctor?: string[]
}

export type FlowConfigShape = {
  flow?: Record<string, unknown>
  lin?: Record<string, unknown>
  ai?: Record<string, unknown>
  hive?: Record<string, unknown>
  zerg?: Record<string, unknown>
  extensions?: string[]
}

export function defineFlowConfig<T extends FlowConfigShape>(config: T): T {
  return config
}

export function defineFlowExtension<T extends FlowExtensionResult>(extension: T): T {
  return extension
}
"#;

const HIVE_DEPRECATION_WARNING: &str =
    "hive config is deprecated and no longer applied to ~/.hive/config.json";

pub fn run(cmd: ConfigCommand) -> Result<()> {
    match cmd.action {
        ConfigAction::Build { json } => {
            let snapshot = build_snapshot()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
            } else {
                print_build_summary(&snapshot);
            }
        }
        ConfigAction::Apply { json } => {
            let snapshot = apply_snapshot()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
            } else {
                print_apply_summary(&snapshot);
            }
        }
        ConfigAction::Doctor { json } => {
            let report = doctor_report();
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_doctor_report(&report);
            }
        }
        ConfigAction::Eval { json } => {
            let snapshot = load_snapshot().or_else(|_| build_snapshot())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
            } else {
                print_eval_summary(&snapshot);
            }
        }
    }
    Ok(())
}

pub fn flow_root_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".flow")
}

pub fn flow_root_config_path() -> PathBuf {
    flow_root_dir().join("config.ts")
}

fn flow_extensions_root() -> PathBuf {
    flow_root_dir().join("extensions")
}

fn flow_runtime_dir() -> PathBuf {
    flow_root_dir().join("runtime")
}

fn legacy_lin_source_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("config/i/lin/config.ts")
}

fn generated_root() -> PathBuf {
    config::global_config_dir().join("generated")
}

fn generated_snapshot_path() -> PathBuf {
    generated_root().join("config.snapshot.json")
}

fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))
}

fn find_ts_runner() -> Result<TsRunner> {
    if let Ok(bin) = which("bun") {
        return Ok(TsRunner {
            display: format!("{} run", bin.display()),
            bin: bin.to_string_lossy().into_owned(),
            args: vec!["run".into()],
        });
    }
    if let Ok(bin) = which("tsx") {
        return Ok(TsRunner {
            display: bin.display().to_string(),
            bin: bin.to_string_lossy().into_owned(),
            args: Vec::new(),
        });
    }
    if let Ok(bin) = which("npx") {
        return Ok(TsRunner {
            display: format!("{} --yes tsx@latest", bin.display()),
            bin: bin.to_string_lossy().into_owned(),
            args: vec!["--yes".into(), "tsx@latest".into()],
        });
    }
    bail!("bun, tsx, or npx is required to load ~/.flow/config.ts")
}

fn load_json_from_ts<T>(path: &Path) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    if !path.exists() {
        bail!("missing TypeScript config: {}", path.display());
    }

    let runner = find_ts_runner()?;
    let mut loader = NamedTempFile::new().context("failed to create temp TS loader")?;
    loader
        .write_all(TS_CONFIG_LOADER.as_bytes())
        .context("failed to write temp TS loader")?;
    let loader_path = loader.into_temp_path();

    let output = Command::new(&runner.bin)
        .args(&runner.args)
        .arg(loader_path.as_os_str())
        .arg(path.as_os_str())
        .output()
        .with_context(|| {
            format!(
                "failed to execute {} for {}",
                runner.display,
                path.display()
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{} exited with {} while loading {}: {}",
            runner.display,
            output.status,
            path.display(),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim())
        .with_context(|| format!("failed to parse JSON output from {}", path.display()))
}

fn legacy_lin_to_root(legacy: LegacyLinSourceConfig) -> RootTsConfig {
    let mut lin_map = serde_json::Map::new();
    if let Some(intents) = legacy.intents {
        lin_map.insert("intents".to_string(), intents);
    }
    if let Some(services) = legacy.services {
        lin_map.insert("services".to_string(), services);
    }
    if let Some(mac) = legacy.mac {
        lin_map.insert("mac".to_string(), mac);
    }
    if let Some(servers) = legacy.servers {
        lin_map.insert("servers".to_string(), servers);
    }
    if let Some(global_tools) = legacy.global_tools {
        lin_map.insert("globalTools".to_string(), global_tools);
    }
    if let Some(watchers) = legacy.watchers {
        lin_map.insert("watchers".to_string(), watchers);
    }

    RootTsConfig {
        flow: legacy.flow,
        lin: if lin_map.is_empty() {
            None
        } else {
            Some(Value::Object(lin_map))
        },
        ai: legacy.ai,
        hive: legacy.hive,
        zerg: None,
        extensions: vec!["lin-compat".to_string()],
    }
}

fn load_root_config() -> Result<(RootTsConfig, PathBuf, Vec<String>)> {
    let root_path = flow_root_config_path();
    if root_path.exists() {
        let config = load_json_from_ts(&root_path)?;
        return Ok((config, root_path, Vec::new()));
    }

    let legacy_path = legacy_lin_source_path();
    if legacy_path.exists() {
        let legacy: LegacyLinSourceConfig = load_json_from_ts(&legacy_path)?;
        return Ok((
            legacy_lin_to_root(legacy),
            legacy_path,
            vec![
                "using legacy ~/config/i/lin/config.ts source because ~/.flow/config.ts is missing"
                    .to_string(),
            ],
        ));
    }

    bail!(
        "missing ~/.flow/config.ts and no legacy source found at {}",
        legacy_path.display()
    )
}

fn discover_extension_files() -> Result<BTreeMap<String, PathBuf>> {
    let root = flow_extensions_root();
    let mut discovered = BTreeMap::new();
    if !root.is_dir() {
        return Ok(discovered);
    }
    for entry in
        fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) if !name.trim().is_empty() => name.to_string(),
            _ => continue,
        };
        for candidate in ["extension.ts", "config.ts"] {
            let candidate_path = path.join(candidate);
            if candidate_path.is_file() {
                discovered.insert(name.clone(), candidate_path);
                break;
            }
        }
    }
    Ok(discovered)
}

pub fn discover_extensions() -> Result<Vec<ExtensionStatus>> {
    let (root, _, _) = load_root_config()?;
    let enabled = root.extensions.into_iter().collect::<BTreeSet<_>>();
    let discovered = discover_extension_files()?;
    let mut extensions = Vec::new();

    if enabled.contains("lin-compat") {
        extensions.push(ExtensionStatus {
            name: "lin-compat".to_string(),
            enabled: true,
            kind: "builtin".to_string(),
            source_path: None,
        });
    }

    for (name, path) in discovered {
        extensions.push(ExtensionStatus {
            name: name.clone(),
            enabled: enabled.contains(&name),
            kind: "filesystem".to_string(),
            source_path: Some(path.display().to_string()),
        });
    }

    extensions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(extensions)
}

fn merge_json_value(base: Option<Value>, overlay: Option<Value>) -> Option<Value> {
    match (base, overlay) {
        (None, None) => None,
        (Some(base), None) => Some(base),
        (None, Some(overlay)) => Some(overlay),
        (Some(mut base), Some(overlay)) => {
            merge_json_in_place(&mut base, overlay);
            Some(base)
        }
    }
}

fn merge_json_in_place(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, value) in overlay_map {
                if let Some(existing) = base_map.get_mut(&key) {
                    merge_json_in_place(existing, value);
                } else {
                    base_map.insert(key, value);
                }
            }
        }
        (base_slot, overlay) => {
            *base_slot = overlay;
        }
    }
}

fn merge_ts_flow(
    base: Option<TsFlowConfig>,
    overlay: Option<TsFlowConfig>,
) -> Result<Option<TsFlowConfig>> {
    let Some(overlay) = overlay else {
        return Ok(base);
    };
    let Some(base) = base else {
        return Ok(Some(overlay));
    };
    let mut base_value = serde_json::to_value(base)?;
    let overlay_value = serde_json::to_value(overlay)?;
    merge_json_in_place(&mut base_value, overlay_value);
    Ok(Some(serde_json::from_value(base_value)?))
}

fn build_effective_config(
    root: &RootTsConfig,
    resolved: &[(ResolvedExtension, FlowExtensionConfig)],
) -> Result<EffectiveFlowConfig> {
    let mut effective = EffectiveFlowConfig {
        flow: root.flow.clone(),
        lin: root.lin.clone(),
        ai: root.ai.clone(),
        hive: root.hive.clone(),
        zerg: root.zerg.clone(),
    };
    for (_, extension) in resolved {
        effective.flow = merge_ts_flow(effective.flow.take(), extension.flow.clone())?;
        effective.lin = merge_json_value(effective.lin.take(), extension.lin.clone());
        effective.ai = merge_json_value(effective.ai.take(), extension.ai.clone());
        effective.hive = merge_json_value(effective.hive.take(), extension.hive.clone());
        effective.zerg = merge_json_value(effective.zerg.take(), extension.zerg.clone());
    }
    Ok(effective)
}

fn generated_flow_config_ts(flow: Option<&TsFlowConfig>) -> Result<String> {
    Ok(format!(
        "// Generated by Flow. Edit ~/.flow/config.ts instead.\nexport default {{\n  flow: {}\n}}\n",
        serde_json::to_string_pretty(&flow.cloned().unwrap_or_default())?
            .lines()
            .collect::<Vec<_>>()
            .join("\n  ")
    ))
}

fn generated_root_ts_module(value: &Value) -> Result<String> {
    Ok(format!(
        "// Generated by Flow. Edit ~/.flow/config.ts instead.\nexport default {}\n",
        serde_json::to_string_pretty(value)?
    ))
}

fn generated_root_config_ts(
    effective: &EffectiveFlowConfig,
    enabled_extensions: &[String],
) -> Result<String> {
    let mut root = serde_json::Map::new();
    if let Some(flow) = effective.flow.as_ref() {
        root.insert("flow".to_string(), serde_json::to_value(flow)?);
    }
    if let Some(lin) = effective.lin.as_ref() {
        root.insert("lin".to_string(), lin.clone());
    }
    if let Some(ai) = effective.ai.as_ref() {
        root.insert("ai".to_string(), ai.clone());
    }
    if let Some(hive) = effective.hive.as_ref() {
        root.insert("hive".to_string(), hive.clone());
    }
    if let Some(zerg) = effective.zerg.as_ref() {
        root.insert("zerg".to_string(), zerg.clone());
    }
    if !enabled_extensions.is_empty() {
        root.insert(
            "extensions".to_string(),
            Value::Array(
                enabled_extensions
                    .iter()
                    .map(|name| Value::String(name.clone()))
                    .collect(),
            ),
        );
    }
    Ok(format!(
        "// Generated by Flow from the legacy config source.\n// Edit this file going forward; Flow will keep consumer configs in sync.\nimport {{ defineFlowConfig }} from \"./runtime/flow-config\"\n\nexport default defineFlowConfig({})\n",
        serde_json::to_string_pretty(&Value::Object(root))?
    ))
}

fn generated_json_file(value: &Value) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(value)?))
}

fn generated_toml_file(value: &Value) -> Result<String> {
    let rendered = toml::to_string_pretty(value).context("failed to render TOML")?;
    Ok(rendered)
}

fn append_hive_deprecation_warning(warnings: &mut Vec<String>, effective: &EffectiveFlowConfig) {
    if effective.hive.is_some()
        && !warnings
            .iter()
            .any(|warning| warning == HIVE_DEPRECATION_WARNING)
    {
        warnings.push(HIVE_DEPRECATION_WARNING.to_string());
    }
}

fn add_artifact(
    artifacts: &mut Vec<GeneratedArtifact>,
    seen_apply_paths: &mut BTreeSet<String>,
    generated_path: PathBuf,
    apply_path: PathBuf,
    source: impl Into<String>,
    content: String,
) -> Result<()> {
    let apply_key = apply_path.display().to_string();
    if !seen_apply_paths.insert(apply_key.clone()) {
        bail!("generated artifact collision for {}", apply_key);
    }
    let id = format!("artifact-{}", artifacts.len() + 1);
    artifacts.push(GeneratedArtifact {
        id,
        generated_path: generated_path.display().to_string(),
        apply_path: apply_key,
        source: source.into(),
        content,
    });
    Ok(())
}

fn build_generated_artifacts(
    effective: &EffectiveFlowConfig,
    resolved: &[(ResolvedExtension, FlowExtensionConfig)],
    enabled_extensions: &[String],
    emit_root_scaffold: bool,
) -> Result<Vec<GeneratedArtifact>> {
    let generated = generated_root();
    let mut artifacts = Vec::new();
    let mut seen_apply_paths = BTreeSet::new();

    add_artifact(
        &mut artifacts,
        &mut seen_apply_paths,
        generated.join("runtime").join("flow-config.ts"),
        flow_runtime_dir().join("flow-config.ts"),
        "runtime-helper",
        FLOW_CONFIG_RUNTIME_HELPER.to_string(),
    )?;

    if emit_root_scaffold {
        add_artifact(
            &mut artifacts,
            &mut seen_apply_paths,
            generated.join("root").join("config.ts"),
            flow_root_config_path(),
            "root-migration",
            generated_root_config_ts(effective, enabled_extensions)?,
        )?;
    }

    add_artifact(
        &mut artifacts,
        &mut seen_apply_paths,
        generated.join("flow").join("config.ts"),
        config::global_config_dir().join("config.ts"),
        "flow",
        generated_flow_config_ts(effective.flow.as_ref())?,
    )?;

    if let Some(ai) = effective.ai.as_ref() {
        add_artifact(
            &mut artifacts,
            &mut seen_apply_paths,
            generated.join("ai").join("config.json"),
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config/ai/config.json"),
            "ai",
            generated_json_file(&json!({ "ai": ai }))?,
        )?;
    }

    if let Some(zerg) = effective.zerg.as_ref() {
        add_artifact(
            &mut artifacts,
            &mut seen_apply_paths,
            generated.join("zerg").join("fleet.json"),
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config/zerg/fleet.json"),
            "zerg",
            generated_json_file(zerg)?,
        )?;
    }

    if let Some(lin) = effective.lin.as_ref() {
        add_artifact(
            &mut artifacts,
            &mut seen_apply_paths,
            generated.join("lin").join("config.ts"),
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config/lin/config.ts"),
            "lin",
            generated_root_ts_module(lin)?,
        )?;

        if enabled_extensions.iter().any(|name| name == "lin-compat") {
            if let Some(intents) = lin.get("intents") {
                add_artifact(
                    &mut artifacts,
                    &mut seen_apply_paths,
                    generated.join("lin").join("intents.json"),
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join(".lin/intents.json"),
                    "lin-compat:intents",
                    generated_json_file(intents)?,
                )?;
            }
            if let Some(mac) = lin.get("mac") {
                add_artifact(
                    &mut artifacts,
                    &mut seen_apply_paths,
                    generated.join("lin").join("mac.toml"),
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("."))
                        .join(".config/lin/mac.toml"),
                    "lin-compat:mac",
                    generated_toml_file(mac)?,
                )?;
            }
        }
    }

    for (resolved_ext, extension) in resolved {
        for (index, generated_file) in extension.generated_files.iter().enumerate() {
            let apply_path = config::expand_path(&generated_file.path);
            let file_name = apply_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("generated.txt");
            let safe_name = resolved_ext.name.replace('/', "-");
            let generated_path = generated.join("extensions").join(safe_name).join(format!(
                "{:02}-{}",
                index + 1,
                file_name
            ));
            add_artifact(
                &mut artifacts,
                &mut seen_apply_paths,
                generated_path,
                apply_path,
                format!("extension:{}", resolved_ext.name),
                generated_file.content.clone(),
            )?;
        }
    }

    Ok(artifacts)
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let dir = path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut temp = NamedTempFile::new_in(&dir)
        .with_context(|| format!("failed to stage {}", path.display()))?;
    temp.write_all(content.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    temp.persist(path)
        .map_err(|err| anyhow::anyhow!("failed to persist {}: {}", path.display(), err.error))?;
    Ok(())
}

fn prune_empty_generated_parents(mut current: Option<&Path>, root: &Path) -> Result<()> {
    while let Some(path) = current {
        if path == root || !path.starts_with(root) || !path.exists() {
            break;
        }
        let is_empty = fs::read_dir(path)
            .with_context(|| format!("failed to read {}", path.display()))?
            .next()
            .is_none();
        if !is_empty {
            break;
        }
        fs::remove_dir(path).with_context(|| format!("failed to remove {}", path.display()))?;
        current = path.parent();
    }
    Ok(())
}

fn prune_stale_generated_artifacts(
    root: &Path,
    previous: &[GeneratedArtifact],
    current: &[GeneratedArtifact],
) -> Result<()> {
    let current_paths = current
        .iter()
        .map(|artifact| PathBuf::from(&artifact.generated_path))
        .collect::<BTreeSet<_>>();

    for artifact in previous {
        let path = PathBuf::from(&artifact.generated_path);
        if current_paths.contains(&path) || !path.starts_with(root) || !path.exists() {
            continue;
        }
        if path.is_dir() {
            fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        } else {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            prune_empty_generated_parents(path.parent(), root)?;
        }
    }

    Ok(())
}

fn write_generated_snapshot(snapshot: &ConfigSnapshot) -> Result<()> {
    let root = generated_root();
    ensure_dir(&root)?;
    if let Ok(previous) = load_snapshot() {
        prune_stale_generated_artifacts(
            &root,
            &previous.generated_artifacts,
            &snapshot.generated_artifacts,
        )?;
    }
    for artifact in &snapshot.generated_artifacts {
        write_atomic(Path::new(&artifact.generated_path), &artifact.content)?;
    }
    write_atomic(
        &generated_snapshot_path(),
        &format!("{}\n", serde_json::to_string_pretty(snapshot)?),
    )?;
    Ok(())
}

pub fn build_snapshot() -> Result<ConfigSnapshot> {
    let (root, source_path, mut warnings) = load_root_config()?;
    let enabled_extensions = root.extensions.clone();
    let discovered = discover_extension_files()?;
    let mut resolved = Vec::new();

    for name in &enabled_extensions {
        if name == "lin-compat" {
            resolved.push((
                ResolvedExtension {
                    name: name.clone(),
                    kind: "builtin".to_string(),
                    source_path: None,
                },
                FlowExtensionConfig {
                    name: Some(name.clone()),
                    ..FlowExtensionConfig::default()
                },
            ));
            continue;
        }
        let Some(path) = discovered.get(name) else {
            bail!(
                "enabled extension {} not found under {}",
                name,
                flow_extensions_root().display()
            );
        };
        let ext_cfg: FlowExtensionConfig = load_json_from_ts(path)?;
        resolved.push((
            ResolvedExtension {
                name: ext_cfg
                    .name
                    .clone()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| name.clone()),
                kind: "filesystem".to_string(),
                source_path: Some(path.display().to_string()),
            },
            ext_cfg,
        ));
    }

    let effective = build_effective_config(&root, &resolved)?;
    append_hive_deprecation_warning(&mut warnings, &effective);
    let generated_artifacts = build_generated_artifacts(
        &effective,
        &resolved,
        &enabled_extensions,
        source_path != flow_root_config_path(),
    )?;
    let snapshot = ConfigSnapshot {
        built_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|dur| dur.as_secs())
            .unwrap_or(0),
        root_config_path: source_path.display().to_string(),
        generated_dir: generated_root().display().to_string(),
        enabled_extensions,
        resolved_extensions: resolved.iter().map(|(meta, _)| meta.clone()).collect(),
        warnings,
        effective,
        generated_artifacts,
    };
    write_generated_snapshot(&snapshot)?;
    Ok(snapshot)
}

pub fn load_snapshot() -> Result<ConfigSnapshot> {
    let path = generated_snapshot_path();
    let text =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn apply_snapshot() -> Result<ConfigSnapshot> {
    let snapshot = build_snapshot()?;
    for artifact in &snapshot.generated_artifacts {
        write_atomic(Path::new(&artifact.apply_path), &artifact.content)?;
    }
    Ok(snapshot)
}

fn doctor_report() -> DoctorReport {
    let root_path = flow_root_config_path();
    let legacy_path = legacy_lin_source_path();
    let snapshot_path = generated_snapshot_path();
    let ts_runner = find_ts_runner().ok().map(|runner| runner.display);
    let extension_dirs = discover_extension_files()
        .unwrap_or_default()
        .into_iter()
        .map(|(name, path)| format!("{} -> {}", name, path.display()))
        .collect::<Vec<_>>();
    let mut warnings = Vec::new();
    if !root_path.exists() {
        if legacy_path.exists() {
            warnings.push(format!(
                "root config missing at {}; legacy source is available at {}",
                root_path.display(),
                legacy_path.display()
            ));
        } else {
            warnings.push(format!("missing root config at {}", root_path.display()));
        }
    }
    if ts_runner.is_none() {
        warnings.push("no TypeScript runner found (need bun, tsx, or npx)".to_string());
    }
    if let Ok((root, _, _)) = load_root_config() {
        let effective = EffectiveFlowConfig {
            flow: root.flow,
            lin: root.lin,
            ai: root.ai,
            hive: root.hive,
            zerg: root.zerg,
        };
        append_hive_deprecation_warning(&mut warnings, &effective);
    }
    DoctorReport {
        root_config_path: root_path.display().to_string(),
        root_config_exists: root_path.exists(),
        generated_snapshot_path: snapshot_path.display().to_string(),
        generated_snapshot_exists: snapshot_path.exists(),
        ts_runner,
        extension_dirs,
        warnings,
    }
}

fn print_build_summary(snapshot: &ConfigSnapshot) {
    println!("Built Flow config snapshot");
    println!("  Root: {}", snapshot.root_config_path);
    println!("  Generated: {}", snapshot.generated_dir);
    println!("  Extensions: {}", snapshot.enabled_extensions.len());
    println!("  Artifacts: {}", snapshot.generated_artifacts.len());
    for artifact in &snapshot.generated_artifacts {
        println!("  - {} -> {}", artifact.source, artifact.generated_path);
    }
    if !snapshot.warnings.is_empty() {
        println!("\nWarnings:");
        for warning in &snapshot.warnings {
            println!("  - {}", warning);
        }
    }
}

fn print_apply_summary(snapshot: &ConfigSnapshot) {
    println!("Applied Flow-generated compatibility outputs");
    for artifact in &snapshot.generated_artifacts {
        println!("  {} -> {}", artifact.generated_path, artifact.apply_path);
    }
    if !snapshot.warnings.is_empty() {
        println!("\nWarnings:");
        for warning in &snapshot.warnings {
            println!("  - {}", warning);
        }
    }
}

fn print_doctor_report(report: &DoctorReport) {
    println!("Flow config doctor");
    println!("  Root config: {}", report.root_config_path);
    println!(
        "  TS runner: {}",
        report.ts_runner.as_deref().unwrap_or("missing")
    );
    println!(
        "  Snapshot: {}",
        if report.generated_snapshot_exists {
            report.generated_snapshot_path.as_str()
        } else {
            "missing"
        }
    );
    println!("  Extension dirs: {}", report.extension_dirs.len());
    for extension in &report.extension_dirs {
        println!("  - {}", extension);
    }
    if !report.warnings.is_empty() {
        println!("\nWarnings:");
        for warning in &report.warnings {
            println!("  - {}", warning);
        }
    }
}

fn print_eval_summary(snapshot: &ConfigSnapshot) {
    println!("Flow config eval");
    println!("  Root: {}", snapshot.root_config_path);
    println!(
        "  Enabled extensions: {}",
        snapshot.enabled_extensions.len()
    );
    println!(
        "  Sections: flow={} lin={} ai={} hive={} zerg={}",
        snapshot.effective.flow.is_some(),
        snapshot.effective.lin.is_some(),
        snapshot.effective.ai.is_some(),
        snapshot.effective.hive.is_some(),
        snapshot.effective.zerg.is_some()
    );
    println!(
        "  Generated artifacts: {}",
        snapshot.generated_artifacts.len()
    );
    for extension in &snapshot.resolved_extensions {
        let source = extension.source_path.as_deref().unwrap_or("<builtin>");
        println!("  - {} [{}] {}", extension.name, extension.kind, source);
    }
}

fn write_root_config(root: &RootTsConfig) -> Result<()> {
    let effective = EffectiveFlowConfig {
        flow: root.flow.clone(),
        lin: root.lin.clone(),
        ai: root.ai.clone(),
        hive: root.hive.clone(),
        zerg: root.zerg.clone(),
    };
    let content = generated_root_config_ts(&effective, &root.extensions)?;
    write_atomic(
        &flow_runtime_dir().join("flow-config.ts"),
        FLOW_CONFIG_RUNTIME_HELPER,
    )?;
    write_atomic(&flow_root_config_path(), &content)
}

pub fn enable_extension(name: &str) -> Result<()> {
    let (mut root, _, _) = load_root_config()?;
    if name != "lin-compat" {
        let discovered = discover_extension_files()?;
        if !discovered.contains_key(name) {
            bail!(
                "extension {} not found under {}",
                name,
                flow_extensions_root().display()
            );
        }
    }
    if !root.extensions.iter().any(|existing| existing == name) {
        root.extensions.push(name.to_string());
        root.extensions.sort();
        root.extensions.dedup();
    }
    write_root_config(&root)
}

pub fn disable_extension(name: &str) -> Result<()> {
    let (mut root, _, _) = load_root_config()?;
    root.extensions.retain(|existing| existing != name);
    write_root_config(&root)
}

pub fn extension_doctor_report() -> Result<Value> {
    let extensions = discover_extensions()?;
    Ok(json!({
        "rootConfigPath": flow_root_config_path(),
        "extensionsRoot": flow_extensions_root(),
        "runtimeHelperPath": flow_runtime_dir().join("flow-config.ts"),
        "extensions": extensions,
    }))
}

pub fn init_extension(name: &str, force: bool) -> Result<PathBuf> {
    let sanitized = name.trim();
    if sanitized.is_empty() {
        bail!("extension name cannot be empty");
    }
    let extension_dir = flow_extensions_root().join(sanitized);
    ensure_dir(&extension_dir)?;
    let extension_file = extension_dir.join("extension.ts");
    let readme_file = extension_dir.join("README.md");
    if !force && (extension_file.exists() || readme_file.exists()) {
        bail!(
            "extension {} already exists at {} (use --force to overwrite scaffold)",
            sanitized,
            extension_dir.display()
        );
    }
    write_atomic(
        &flow_runtime_dir().join("flow-config.ts"),
        FLOW_CONFIG_RUNTIME_HELPER,
    )?;
    let scaffold = format!(
        "import {{ defineFlowExtension }} from \"../../runtime/flow-config\"\n\nexport default defineFlowExtension({{\n  name: {name:?},\n  doctor: [\"replace this scaffold with real checks\"],\n  generatedFiles: [],\n}})\n"
    );
    write_atomic(&extension_file, &scaffold)?;
    write_atomic(
        &readme_file,
        &format!(
            "# {name}\n\nFlow extension scaffold.\n\n- edit [extension.ts](./extension.ts)\n- enable with `f ext enable {name}`\n- inspect with `f ext list`\n"
        ),
    )?;
    Ok(extension_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_flow(tool: &str) -> TsFlowConfig {
        TsFlowConfig {
            commit: Some(crate::config::TsCommitConfig {
                tool: Some(tool.to_string()),
                ..crate::config::TsCommitConfig::default()
            }),
            ..TsFlowConfig::default()
        }
    }

    fn sample_generated_artifact(path: PathBuf) -> GeneratedArtifact {
        GeneratedArtifact {
            id: "artifact-test".to_string(),
            generated_path: path.display().to_string(),
            apply_path: "/tmp/unused".to_string(),
            source: "test".to_string(),
            content: "content".to_string(),
        }
    }

    #[test]
    fn build_effective_config_merges_extension_overlays() {
        let root = RootTsConfig {
            flow: Some(sample_flow("claude")),
            lin: Some(json!({"watchers":[{"name":"base"}]})),
            extensions: vec!["mac".to_string()],
            ..RootTsConfig::default()
        };
        let resolved = vec![(
            ResolvedExtension {
                name: "mac".to_string(),
                kind: "filesystem".to_string(),
                source_path: Some("/tmp/mac/extension.ts".to_string()),
            },
            FlowExtensionConfig {
                flow: Some(sample_flow("codex")),
                lin: Some(json!({"watchers":[{"name":"overlay"}],"mac":{"model":{"name":"gpt"}}})),
                ..FlowExtensionConfig::default()
            },
        )];

        let effective = build_effective_config(&root, &resolved).expect("effective config");
        assert_eq!(
            effective
                .flow
                .and_then(|flow| flow.commit)
                .and_then(|commit| commit.tool),
            Some("codex".to_string())
        );
        let lin = effective.lin.expect("lin");
        let watchers = lin
            .get("watchers")
            .and_then(Value::as_array)
            .expect("watchers array");
        assert_eq!(watchers.len(), 1);
        assert_eq!(
            watchers[0].get("name").and_then(Value::as_str),
            Some("overlay")
        );
        assert_eq!(
            lin.pointer("/mac/model/name").and_then(Value::as_str),
            Some("gpt")
        );
    }

    #[test]
    fn build_generated_artifacts_includes_lin_split_outputs() {
        let effective = EffectiveFlowConfig {
            flow: Some(sample_flow("codex")),
            lin: Some(json!({
                "servers": [{"name":"lin"}],
                "intents": [{"trigger": "open review"}],
                "mac": {"model": {"name": "gpt-5"}}
            })),
            ai: Some(json!({"agents": {"enabled": ["review"]}})),
            hive: None,
            zerg: None,
        };
        let artifacts =
            build_generated_artifacts(&effective, &[], &["lin-compat".to_string()], false)
                .expect("generated artifacts");

        let apply_paths = artifacts
            .iter()
            .map(|artifact| artifact.apply_path.clone())
            .collect::<Vec<_>>();
        assert!(
            apply_paths
                .iter()
                .any(|path| path.ends_with(".config/flow/config.ts"))
        );
        assert!(
            apply_paths
                .iter()
                .any(|path| path.ends_with(".config/lin/config.ts"))
        );
        assert!(
            apply_paths
                .iter()
                .any(|path| path.ends_with(".lin/intents.json"))
        );
        assert!(
            apply_paths
                .iter()
                .any(|path| path.ends_with(".config/lin/mac.toml"))
        );
        assert!(
            apply_paths
                .iter()
                .any(|path| path.ends_with(".config/ai/config.json"))
        );
    }

    #[test]
    fn build_generated_artifacts_can_emit_root_scaffold() {
        let effective = EffectiveFlowConfig {
            flow: Some(sample_flow("codex")),
            lin: Some(json!({"watchers": []})),
            ai: None,
            hive: None,
            zerg: None,
        };
        let artifacts =
            build_generated_artifacts(&effective, &[], &["lin-compat".to_string()], true)
                .expect("generated artifacts");
        assert!(
            artifacts
                .iter()
                .any(|artifact| artifact.apply_path.ends_with(".flow/config.ts"))
        );
    }

    #[test]
    fn build_generated_artifacts_does_not_emit_hive_output() {
        let effective = EffectiveFlowConfig {
            flow: Some(sample_flow("codex")),
            lin: None,
            ai: None,
            hive: Some(json!({"agents": {"shell": {}}})),
            zerg: None,
        };
        let artifacts =
            build_generated_artifacts(&effective, &[], &["lin-compat".to_string()], false)
                .expect("generated artifacts");
        assert!(
            !artifacts
                .iter()
                .any(|artifact| artifact.apply_path.ends_with(".hive/config.json"))
        );
    }

    #[test]
    fn append_hive_deprecation_warning_adds_warning_once() {
        let effective = EffectiveFlowConfig {
            hive: Some(json!({"agents": {"shell": {}}})),
            ..EffectiveFlowConfig::default()
        };
        let mut warnings = Vec::new();
        append_hive_deprecation_warning(&mut warnings, &effective);
        append_hive_deprecation_warning(&mut warnings, &effective);
        assert_eq!(warnings, vec![HIVE_DEPRECATION_WARNING.to_string()]);
    }

    #[test]
    fn generated_artifact_collisions_are_errors() {
        let resolved = vec![(
            ResolvedExtension {
                name: "dup".to_string(),
                kind: "filesystem".to_string(),
                source_path: Some("/tmp/dup/extension.ts".to_string()),
            },
            FlowExtensionConfig {
                generated_files: vec![
                    ExtensionGeneratedFile {
                        path: "~/.config/example/test.txt".to_string(),
                        content: "one".to_string(),
                    },
                    ExtensionGeneratedFile {
                        path: "~/.config/example/test.txt".to_string(),
                        content: "two".to_string(),
                    },
                ],
                ..FlowExtensionConfig::default()
            },
        )];
        let err = build_generated_artifacts(&EffectiveFlowConfig::default(), &resolved, &[], false)
            .expect_err("collision should fail");
        assert!(err.to_string().contains("generated artifact collision"));
    }

    #[test]
    fn prune_stale_generated_artifacts_removes_retired_outputs_under_generated_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("generated");
        let stale = root.join("hive").join("config.json");
        let keep = root.join("flow").join("config.ts");

        fs::create_dir_all(stale.parent().expect("stale parent")).expect("create stale parent");
        fs::create_dir_all(keep.parent().expect("keep parent")).expect("create keep parent");
        fs::write(&stale, "{}\n").expect("write stale file");
        fs::write(&keep, "export default {}\n").expect("write keep file");

        let previous = vec![
            sample_generated_artifact(stale.clone()),
            sample_generated_artifact(keep.clone()),
        ];
        let current = vec![sample_generated_artifact(keep.clone())];

        prune_stale_generated_artifacts(&root, &previous, &current).expect("prune stale outputs");

        assert!(!stale.exists(), "stale generated file should be removed");
        assert!(
            !root.join("hive").exists(),
            "empty generated directory should be removed"
        );
        assert!(keep.exists(), "current generated file should remain");
    }

    #[test]
    fn prune_stale_generated_artifacts_does_not_remove_paths_outside_generated_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("generated");
        let outside = temp.path().join("outside").join("config.json");

        fs::create_dir_all(outside.parent().expect("outside parent")).expect("create outside");
        fs::write(&outside, "{}\n").expect("write outside file");

        let previous = vec![sample_generated_artifact(outside.clone())];
        prune_stale_generated_artifacts(&root, &previous, &[]).expect("skip outside file");

        assert!(
            outside.exists(),
            "non-generated files must not be removed during cleanup"
        );
    }
}
