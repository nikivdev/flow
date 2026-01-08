use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::State,
    http::{StatusCode, Uri},
    response::{Html, IntoResponse, Json, Response},
    routing::get,
};
use serde::Serialize;
use tokio::runtime::Runtime;
use which::which;

use crate::ai;
use crate::cli::WebOpts;

#[derive(Clone)]
struct WebState {
    project_root: PathBuf,
    web_root: Option<PathBuf>,
    fallback_index: Option<String>,
}

#[derive(Serialize)]
struct ProjectsResponse {
    projects: Vec<ProjectCard>,
}

#[derive(Serialize)]
struct AiTreeResponse {
    entries: Vec<AiEntry>,
}

#[derive(Serialize)]
struct SessionsResponse {
    sessions: Vec<ai::WebSession>,
}

#[derive(Serialize)]
struct ProjectCard {
    name: String,
    path: String,
    path_url: String,
    summary: Option<String>,
    openapi: Option<OpenApiSpec>,
    ai_entries: Vec<AiEntry>,
    status: String,
}

#[derive(Serialize)]
struct OpenApiSpec {
    path: String,
    url: String,
    format: String,
}

#[derive(Serialize)]
struct AiEntry {
    path: String,
    kind: String,
}

pub fn run(opts: WebOpts) -> Result<()> {
    let project_root = std::env::current_dir()?;
    ensure_web_ui(&project_root)?;
    build_web_ui(&project_root)?;
    let (web_root, fallback_index) = resolve_web_root(&project_root);

    let host = opts.host;
    let port = opts.port;
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .context("invalid host:port")?;

    let state = WebState {
        project_root: project_root.clone(),
        web_root,
        fallback_index,
    };

    let rt = Runtime::new().context("failed to create tokio runtime")?;
    rt.block_on(async move {
        let app = Router::new()
            .route("/api/projects", get(projects))
            .route("/api/ai", get(ai_tree))
            .route("/api/sessions", get(sessions))
            .route("/api/openapi", get(openapi))
            .route("/", get(index))
            .fallback(fallback)
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .context("failed to bind web server")?;

        let url = format!("http://{host}:{port}");
        open_in_browser(&url)?;
        println!("Flow web running at {url}");

        axum::serve(listener, app)
            .await
            .context("web server error")?;

        Ok(())
    })
}

async fn index(State(state): State<WebState>) -> Result<Html<String>, (StatusCode, String)> {
    if let Some(html) = &state.fallback_index {
        return Ok(Html(html.clone()));
    }
    let web_root = state
        .web_root
        .as_ref()
        .ok_or((StatusCode::NOT_FOUND, "missing web root".to_string()))?;
    let index_path = web_root.join("index.html");
    let html = fs::read_to_string(&index_path)
        .map_err(|err| (StatusCode::NOT_FOUND, format!("missing index.html: {err}")))?;
    Ok(Html(html))
}

async fn fallback(
    State(state): State<WebState>,
    uri: Uri,
) -> Result<Response, (StatusCode, String)> {
    let Some(web_root) = &state.web_root else {
        return index(State(state)).await.map(|html| html.into_response());
    };
    let path = uri.path().trim_start_matches('/');
    if Path::new(path)
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err((StatusCode::NOT_FOUND, "not found".to_string()));
    }
    let file_path = web_root.join(path);
    if file_path.is_file() {
        let contents =
            fs::read(&file_path).map_err(|err| (StatusCode::NOT_FOUND, err.to_string()))?;
        let content_type = content_type_for_path(&file_path);
        return Ok((
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, content_type)],
            contents,
        )
            .into_response());
    }
    index(State(state)).await.map(|html| html.into_response())
}

async fn projects(State(state): State<WebState>) -> Result<Json<ProjectsResponse>, StatusCode> {
    let project = build_project_card(&state.project_root);
    Ok(Json(ProjectsResponse {
        projects: vec![project],
    }))
}

async fn ai_tree(State(state): State<WebState>) -> Result<Json<AiTreeResponse>, StatusCode> {
    let entries = list_ai_tree_entries(&state.project_root.join(".ai"));
    Ok(Json(AiTreeResponse { entries }))
}

async fn sessions(State(state): State<WebState>) -> Result<Json<SessionsResponse>, StatusCode> {
    let sessions = ai::get_sessions_for_web(&state.project_root)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(SessionsResponse { sessions }))
}

async fn openapi(State(state): State<WebState>) -> Result<impl IntoResponse, StatusCode> {
    let Some((path, format)) = find_openapi_spec(&state.project_root) else {
        return Err(StatusCode::NOT_FOUND);
    };
    let contents = fs::read(&path).map_err(|_| StatusCode::NOT_FOUND)?;
    let content_type = if format == "json" {
        "application/json"
    } else {
        "application/yaml"
    };
    Ok(([(axum::http::header::CONTENT_TYPE, content_type)], contents))
}

fn build_project_card(project_root: &Path) -> ProjectCard {
    let name = project_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_string();

    let summary = read_first_line(project_root.join("readme.md"))
        .or_else(|| read_first_line(project_root.join("README.md")));

    let openapi = find_openapi_spec(project_root).map(|(path, format)| {
        let rel = path.strip_prefix(project_root).unwrap_or(&path);
        OpenApiSpec {
            path: rel.to_string_lossy().to_string(),
            url: "/api/openapi".to_string(),
            format,
        }
    });

    let ai_entries = list_ai_top_entries(&project_root.join(".ai"));

    let has_openapi = openapi.is_some();

    ProjectCard {
        name,
        path: project_root.display().to_string(),
        path_url: format!("file://{}", project_root.display()),
        summary,
        openapi,
        ai_entries,
        status: if has_openapi { "OpenAPI" } else { "Ready" }.to_string(),
    }
}

fn read_first_line(path: PathBuf) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return Some(trimmed.to_string());
    }
    None
}

fn find_openapi_spec(project_root: &Path) -> Option<(PathBuf, String)> {
    let candidates = [
        (project_root.join("openapi.json"), "json"),
        (project_root.join("openapi.yaml"), "yaml"),
        (project_root.join("openapi.yml"), "yaml"),
        (project_root.join("spec/openapi.json"), "json"),
        (project_root.join("spec/openapi.yaml"), "yaml"),
        (project_root.join("spec/openapi.yml"), "yaml"),
        (project_root.join("docs/openapi.json"), "json"),
        (project_root.join("docs/openapi.yaml"), "yaml"),
        (project_root.join("docs/openapi.yml"), "yaml"),
        (project_root.join("openapi/openapi.json"), "json"),
        (project_root.join("openapi/openapi.yaml"), "yaml"),
        (project_root.join("openapi/openapi.yml"), "yaml"),
        (project_root.join(".ai/openapi.json"), "json"),
        (project_root.join(".ai/openapi.yaml"), "yaml"),
        (project_root.join(".ai/openapi.yml"), "yaml"),
    ];

    candidates
        .into_iter()
        .find(|(path, _)| path.exists())
        .map(|(path, format)| (path, format.to_string()))
}

fn list_ai_top_entries(ai_root: &Path) -> Vec<AiEntry> {
    let mut entries = Vec::new();
    let dir = match fs::read_dir(ai_root) {
        Ok(dir) => dir,
        Err(_) => return entries,
    };

    for entry in dir.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        let kind = if path.is_dir() { "dir" } else { "file" };
        entries.push(AiEntry {
            path: name,
            kind: kind.to_string(),
        });
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

fn list_ai_tree_entries(ai_root: &Path) -> Vec<AiEntry> {
    let mut entries = Vec::new();
    if !ai_root.exists() {
        return entries;
    }

    let mut stack = vec![ai_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let dir_entries = match fs::read_dir(&dir) {
            Ok(dir_entries) => dir_entries,
            Err(_) => continue,
        };

        for entry in dir_entries.flatten() {
            let path = entry.path();
            let rel = match path.strip_prefix(ai_root) {
                Ok(rel) if !rel.as_os_str().is_empty() => rel,
                _ => continue,
            };
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            let file_type = metadata.file_type();
            let kind = if file_type.is_symlink() {
                "symlink"
            } else if file_type.is_dir() {
                "dir"
            } else {
                "file"
            };
            entries.push(AiEntry {
                path: rel.to_string_lossy().to_string(),
                kind: kind.to_string(),
            });

            if file_type.is_dir() && !file_type.is_symlink() {
                stack.push(path);
            }
        }
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

fn content_type_for_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        _ => "application/octet-stream",
    }
}

fn build_web_ui(project_root: &Path) -> Result<()> {
    let web_root = project_root.join(".ai").join("web");
    let package_json = web_root.join("package.json");
    if !package_json.exists() {
        return Ok(());
    }

    if which("bun").is_err() {
        bail!("bun is required to build .ai/web (install bun or remove .ai/web/package.json)");
    }

    let node_modules = web_root.join("node_modules");
    let install_stamp = node_modules.join(".flow-web-install");
    if needs_install(
        &node_modules,
        &package_json,
        &web_root.join("bun.lock"),
        &install_stamp,
    )? {
        run_command("bun", &["install"], &web_root).context("bun install failed for .ai/web")?;
        write_install_stamp(&install_stamp)?;
    }

    run_command("bun", &["run", "build"], &web_root).context("bun run build failed for .ai/web")?;

    Ok(())
}

fn needs_install(
    node_modules: &Path,
    package_json: &Path,
    bun_lock: &Path,
    install_stamp: &Path,
) -> Result<bool> {
    if !node_modules.exists() {
        return Ok(true);
    }
    if !install_stamp.exists() {
        return Ok(true);
    }
    if is_newer(package_json, install_stamp)? {
        return Ok(true);
    }
    if bun_lock.exists() && is_newer(bun_lock, install_stamp)? {
        return Ok(true);
    }
    Ok(false)
}

fn is_newer(path: &Path, stamp: &Path) -> Result<bool> {
    let path_time = file_modified(path)?;
    let stamp_time = file_modified(stamp)?;
    Ok(path_time > stamp_time)
}

fn file_modified(path: &Path) -> Result<SystemTime> {
    let metadata = fs::metadata(path)?;
    Ok(metadata.modified()?)
}

fn write_install_stamp(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, b"installed")?;
    Ok(())
}

fn run_command(command: &str, args: &[&str], cwd: &Path) -> Result<()> {
    let status = std::process::Command::new(command)
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("failed to spawn {}", command))?;
    if status.success() {
        Ok(())
    } else {
        bail!("{} {:?} exited with {}", command, args, status)
    }
}

#[cfg(target_os = "macos")]
fn open_in_browser(url: &str) -> Result<()> {
    std::process::Command::new("open").arg(url).status()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_in_browser(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open").arg(url).status()?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn open_in_browser(url: &str) -> Result<()> {
    println!("Open this URL in your browser: {url}");
    Ok(())
}

pub fn ensure_web_ui(project_root: &Path) -> Result<()> {
    let web_root = project_root.join(".ai").join("web");
    if !web_root.exists() {
        fs::create_dir_all(&web_root)?;
    }
    let index_path = web_root.join("index.html");
    let has_vite_source = web_root.join("package.json").exists() && web_root.join("src").exists();
    if !index_path.exists() && !has_vite_source {
        fs::write(&index_path, default_web_template())?;
    }
    Ok(())
}

fn resolve_web_root(project_root: &Path) -> (Option<PathBuf>, Option<String>) {
    let web_root = project_root.join(".ai").join("web");
    let dist_root = web_root.join("dist");
    let dist_index = dist_root.join("index.html");
    if dist_index.exists() {
        return (Some(dist_root), None);
    }

    let has_vite_source = web_root.join("package.json").exists() && web_root.join("src").exists();
    if has_vite_source {
        return (None, Some(default_web_template().to_string()));
    }

    let root_index = web_root.join("index.html");
    if root_index.exists() {
        return (Some(web_root), None);
    }

    (None, Some(default_web_template().to_string()))
}

fn default_web_template() -> &'static str {
    r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Flow Web</title>
    <style>
      :root {
        --bg: #0f1114;
        --panel: #171b22;
        --text: #f5f7fb;
        --muted: #9aa3b2;
        --accent: #7ee787;
        --line: rgba(255, 255, 255, 0.08);
      }

      * {
        box-sizing: border-box;
      }

      body {
        margin: 0;
        min-height: 100vh;
        background: radial-gradient(circle at 20% 20%, rgba(126, 231, 135, 0.2), transparent 45%),
          var(--bg);
        color: var(--text);
        font-family: "IBM Plex Sans", "Segoe UI", sans-serif;
      }

      main {
        max-width: 780px;
        margin: 0 auto;
        padding: 64px 24px 72px;
      }

      h1 {
        margin: 0;
        font-size: 2.2rem;
      }

      p {
        margin: 0;
        color: var(--muted);
        font-size: 1.05rem;
        line-height: 1.6;
      }

      .card {
        background: var(--panel);
        border: 1px solid var(--line);
        border-radius: 16px;
        padding: 20px;
        margin-top: 20px;
      }

      code {
        color: var(--accent);
      }
    </style>
  </head>
  <body>
    <main>
      <div class="card">
        <h1>Flow Web UI not built</h1>
        <p>
          Build your Vite app to <code>.ai/web/dist</code> and refresh.
          Example: <code>vite build</code>
        </p>
        <p style="margin-top: 12px;">
          API endpoints are live at:
          <code>/api/projects</code>, <code>/api/ai</code>, <code>/api/openapi</code>
        </p>
      </div>
    </main>
  </body>
</html>
"#
}
