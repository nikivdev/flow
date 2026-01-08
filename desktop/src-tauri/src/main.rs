// Prevents additional console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use serde::Serialize;
use tauri::Manager;

#[derive(Debug, Serialize, Clone)]
struct DesktopProject {
    name: String,
    project_root: String,
    config_path: String,
    updated_ms: u64,
}

#[tauri::command]
fn list_projects() -> Result<Vec<DesktopProject>, String> {
    let entries = flowd::projects::list_projects().map_err(|err| err.to_string())?;
    let projects = entries
        .into_iter()
        .map(|entry| DesktopProject {
            name: entry.name,
            project_root: entry.project_root.to_string_lossy().to_string(),
            config_path: entry.config_path.to_string_lossy().to_string(),
            updated_ms: entry.updated_ms as u64,
        })
        .collect();
    Ok(projects)
}

#[tauri::command]
async fn discover_projects(root: String) -> Result<Vec<DesktopProject>, String> {
    tauri::async_runtime::spawn_blocking(move || discover_projects_sync(root))
        .await
        .map_err(|err| err.to_string())?
}

fn discover_projects_sync(root: String) -> Result<Vec<DesktopProject>, String> {
    let root_path = flowd::config::expand_path(&root);
    if !root_path.exists() {
        return Ok(Vec::new());
    }

    let mut configs = Vec::new();

    let walker = WalkBuilder::new(&root_path)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .max_depth(Some(10))
        .filter_entry(|entry| {
            if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    "node_modules"
                        | "target"
                        | "dist"
                        | "build"
                        | ".git"
                        | ".hg"
                        | ".svn"
                        | "__pycache__"
                        | ".pytest_cache"
                        | ".mypy_cache"
                        | "venv"
                        | ".venv"
                        | "vendor"
                        | "Pods"
                        | ".cargo"
                        | ".rustup"
                )
            } else {
                true
            }
        })
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false)
            && entry.file_name() == "flow.toml"
        {
            configs.push(path.to_path_buf());
        }
    }

    let mut seen = HashSet::new();
    let mut projects = Vec::new();

    for config_path in configs {
        let canonical = config_path
            .canonicalize()
            .unwrap_or_else(|_| config_path.clone());
        let key = canonical.to_string_lossy().to_string();
        if !seen.insert(key.clone()) {
            continue;
        }
        if let Some(project) = build_project_from_config(&canonical) {
            projects.push(project);
        }
    }

    projects.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(projects)
}

#[tauri::command]
fn sessions_for_project(project_root: String) -> Result<Vec<flowd::ai::WebSession>, String> {
    let root = PathBuf::from(project_root);
    flowd::ai::get_sessions_for_web(&root).map_err(|err| err.to_string())
}

#[tauri::command]
fn logs_for_project(
    project: Option<String>,
    since: Option<i64>,
    limit: Option<usize>,
) -> Result<Vec<flowd::log_store::StoredLogEntry>, String> {
    let conn = flowd::log_store::open_log_db().map_err(|err| err.to_string())?;
    let query = flowd::log_store::LogQuery {
        project,
        since,
        limit: limit.unwrap_or(200),
        ..Default::default()
    };
    flowd::log_store::query_logs(&conn, &query).map_err(|err| err.to_string())
}

fn build_project_from_config(config_path: &Path) -> Option<DesktopProject> {
    let config = flowd::config::load(config_path).ok()?;
    let project_root = config_path.parent()?.to_path_buf();
    let name = config
        .project_name
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback_project_name(&project_root));
    Some(DesktopProject {
        name,
        project_root: project_root.to_string_lossy().to_string(),
        config_path: config_path.to_string_lossy().to_string(),
        updated_ms: file_modified_ms(config_path),
    })
}

fn fallback_project_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

fn file_modified_ms(path: &Path) -> u64 {
    path.metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn main() {
    flowd::init_tracing();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.maximize();
                if cfg!(debug_assertions) {
                    window.open_devtools();
                    let _ = window.set_focus();
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_projects,
            discover_projects,
            sessions_for_project,
            logs_for_project
        ])
        .run(tauri::generate_context!())
        .expect("error while running Flow Desktop");
}
