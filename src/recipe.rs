use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use shellexpand::tilde;

use crate::cli::{
    RecipeAction, RecipeCommand, RecipeInitOpts, RecipeListOpts, RecipeRunOpts, RecipeScopeArg,
    RecipeSearchOpts,
};
use crate::config;

const ENV_GLOBAL_RECIPE_DIR: &str = "FLOW_RECIPES_GLOBAL_DIR";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Scope {
    Project,
    Global,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Global => "global",
        }
    }
}

#[derive(Debug, Clone)]
struct Recipe {
    id: String,
    name: String,
    description: String,
    path: PathBuf,
    scope: Scope,
    runner: RecipeRunner,
    tags: Vec<String>,
}

#[derive(Debug, Clone)]
enum RecipeRunner {
    Shell { shell: String, command: String },
    MoonbitFile,
}

#[derive(Debug, Clone, Default)]
struct Frontmatter {
    title: Option<String>,
    description: Option<String>,
    tags: Vec<String>,
}

pub fn run(cmd: RecipeCommand) -> Result<()> {
    eprintln!(
        "warning: `f recipe` is legacy compatibility. Prefer task-centric workflows with flow.toml tasks + .ai/tasks/*.mbt."
    );
    match cmd.action.unwrap_or(RecipeAction::List(RecipeListOpts {
        scope: RecipeScopeArg::All,
        query: None,
        global_dir: None,
    })) {
        RecipeAction::List(opts) => list_recipes(opts),
        RecipeAction::Search(opts) => search_recipes(opts),
        RecipeAction::Run(opts) => run_recipe(opts),
        RecipeAction::Init(opts) => init_recipes(opts),
    }
}

fn list_recipes(opts: RecipeListOpts) -> Result<()> {
    let recipes = load_recipes(opts.scope, opts.global_dir.as_deref())?;
    let filtered = filter_recipes(recipes, opts.query.as_deref());
    if filtered.is_empty() {
        println!("No recipes found.");
        return Ok(());
    }

    for recipe in filtered {
        let tags = if recipe.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", recipe.tags.join(","))
        };
        println!(
            "{:<7} {:<36} {}{}",
            recipe.scope.as_str(),
            recipe.id,
            recipe.name,
            tags
        );
        if !recipe.description.is_empty() {
            println!("         {}", recipe.description);
        }
    }
    Ok(())
}

fn search_recipes(opts: RecipeSearchOpts) -> Result<()> {
    let recipes = load_recipes(opts.scope, opts.global_dir.as_deref())?;
    let filtered = filter_recipes(recipes, Some(opts.query.as_str()));
    if filtered.is_empty() {
        println!("No recipes matched '{}'.", opts.query);
        return Ok(());
    }
    for recipe in filtered {
        println!(
            "{:<7} {:<36} {}",
            recipe.scope.as_str(),
            recipe.id,
            recipe.name
        );
    }
    Ok(())
}

fn run_recipe(opts: RecipeRunOpts) -> Result<()> {
    let recipes = load_recipes(opts.scope, opts.global_dir.as_deref())?;
    let recipe = match select_recipe(&recipes, &opts.selector) {
        Ok(recipe) => recipe,
        Err(err) => {
            eprintln!("{err}");
            bail!("failed to select recipe")
        }
    };

    let cwd = resolve_cwd(opts.cwd.as_deref())?;

    println!(
        "Running recipe {} ({}) from {}",
        recipe.id,
        recipe.scope.as_str(),
        recipe.path.display()
    );
    println!("cwd: {}", cwd.display());
    match &recipe.runner {
        RecipeRunner::Shell { shell, command } => {
            let shell_bin = resolve_shell_bin(shell);
            let shell_cmd = command.trim();
            println!("engine: shell");
            println!("shell: {}", shell_bin);
            println!("cmd: {}", shell_cmd);

            if opts.dry_run {
                return Ok(());
            }

            let status = Command::new(&shell_bin)
                .arg("-lc")
                .arg(shell_cmd)
                .current_dir(&cwd)
                .status()
                .with_context(|| format!("failed to run recipe command via {}", shell_bin))?;

            if !status.success() {
                bail!("recipe '{}' failed with status {}", recipe.id, status);
            }
        }
        RecipeRunner::MoonbitFile => {
            println!("engine: moonbit");
            println!("cmd: moon run {}", recipe.path.display());

            if opts.dry_run {
                return Ok(());
            }

            let status = Command::new("moon")
                .arg("run")
                .arg(&recipe.path)
                .current_dir(&cwd)
                .status()
                .with_context(|| format!("failed to run moon recipe {}", recipe.path.display()))?;

            if !status.success() {
                bail!("recipe '{}' failed with status {}", recipe.id, status);
            }
        }
    }
    Ok(())
}

fn init_recipes(opts: RecipeInitOpts) -> Result<()> {
    let project_root = detect_project_root()?;
    let global_dir = resolve_global_dir(&project_root, opts.global_dir.as_deref());

    let mut created: Vec<PathBuf> = Vec::new();
    let mut created_files: Vec<PathBuf> = Vec::new();

    if matches!(opts.scope, RecipeScopeArg::Project | RecipeScopeArg::All) {
        let project_dir = project_root.join(".ai/recipes/project");
        ensure_dir(&project_dir, &mut created)?;
        write_starter_recipe(
            &project_dir.join("open-safari-new-tab.md"),
            STARTER_PROJECT_RECIPE,
            &mut created_files,
        )?;
        write_starter_recipe(
            &project_dir.join("bridge-latency-bench.md"),
            STARTER_PROJECT_BENCH_RECIPE,
            &mut created_files,
        )?;
        write_starter_recipe(
            &project_dir.join("moonbit-starter.mbt"),
            STARTER_PROJECT_MOONBIT_RECIPE,
            &mut created_files,
        )?;
    }

    if matches!(opts.scope, RecipeScopeArg::Global | RecipeScopeArg::All) {
        ensure_dir(&global_dir, &mut created)?;
        write_starter_recipe(
            &global_dir.join("system-ready-check.md"),
            STARTER_GLOBAL_RECIPE,
            &mut created_files,
        )?;
    }

    if created.is_empty() && created_files.is_empty() {
        println!("Recipe directories already initialized.");
    } else {
        for dir in created {
            println!("created dir: {}", dir.display());
        }
        for file in created_files {
            println!("created recipe: {}", file.display());
        }
    }

    Ok(())
}

fn load_recipes(scope: RecipeScopeArg, global_dir_override: Option<&str>) -> Result<Vec<Recipe>> {
    let project_root = detect_project_root()?;
    let mut roots: Vec<(Scope, PathBuf)> = Vec::new();

    if matches!(scope, RecipeScopeArg::Project | RecipeScopeArg::All) {
        let preferred = project_root.join(".ai/recipes/project");
        if preferred.exists() {
            roots.push((Scope::Project, preferred));
        } else {
            roots.push((Scope::Project, project_root.join(".ai/recipes")));
        }
    }
    if matches!(scope, RecipeScopeArg::Global | RecipeScopeArg::All) {
        roots.push((
            Scope::Global,
            resolve_global_dir(&project_root, global_dir_override),
        ));
    }

    let mut seen = BTreeSet::new();
    roots.retain(|(_, root)| seen.insert(root.clone()));

    let mut recipes = Vec::new();
    let mut seen_recipe_paths = BTreeSet::new();
    for (scope, root) in roots {
        if !root.exists() {
            continue;
        }
        let files = collect_recipe_files(&root)?;
        for file in files {
            let key = (scope, file.clone());
            if !seen_recipe_paths.insert(key) {
                continue;
            }
            if let Some(recipe) = parse_recipe(scope, &root, &file)? {
                recipes.push(recipe);
            }
        }
    }

    recipes.sort_by(|a, b| (a.scope, a.id.as_str()).cmp(&(b.scope, b.id.as_str())));
    Ok(recipes)
}

fn collect_recipe_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_recipe_files_recursive(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_recipe_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", dir.display()))?;
        let path = entry.path();
        let ty = entry
            .file_type()
            .with_context(|| format!("failed to get type for {}", path.display()))?;
        if ty.is_dir() {
            collect_recipe_files_recursive(&path, out)?;
            continue;
        }
        if !ty.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ext == "md" || ext == "markdown" || ext == "mbt" {
            out.push(path);
        }
    }
    Ok(())
}

fn parse_recipe(scope: Scope, root: &Path, path: &Path) -> Result<Option<Recipe>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ext == "mbt" {
        return parse_moonbit_recipe(scope, root, path);
    }
    parse_markdown_recipe(scope, root, path)
}

fn parse_markdown_recipe(scope: Scope, root: &Path, path: &Path) -> Result<Option<Recipe>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let (frontmatter, body) = parse_frontmatter(&content);
    let (shell, command) = match extract_first_shell_block(body) {
        Some(found) => found,
        None => return Ok(None),
    };

    let relative = path.strip_prefix(root).unwrap_or(path);
    let id = format!(
        "{}:{}",
        scope.as_str(),
        relative
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/")
    );

    let title = frontmatter
        .title
        .or_else(|| extract_first_heading(body))
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("recipe")
                .replace('-', " ")
        });
    let description = frontmatter
        .description
        .or_else(|| extract_description(body))
        .unwrap_or_default();

    Ok(Some(Recipe {
        id,
        name: title,
        description,
        path: path.to_path_buf(),
        scope,
        runner: RecipeRunner::Shell { shell, command },
        tags: frontmatter.tags,
    }))
}

fn parse_moonbit_recipe(scope: Scope, root: &Path, path: &Path) -> Result<Option<Recipe>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let frontmatter = parse_moonbit_metadata(&content);

    let relative = path.strip_prefix(root).unwrap_or(path);
    let id = format!(
        "{}:{}",
        scope.as_str(),
        relative
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/")
    );

    let title = frontmatter.title.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("recipe")
            .replace(['-', '_'], " ")
    });
    let description = frontmatter.description.unwrap_or_default();

    Ok(Some(Recipe {
        id,
        name: title,
        description,
        path: path.to_path_buf(),
        scope,
        runner: RecipeRunner::MoonbitFile,
        tags: frontmatter.tags,
    }))
}

fn parse_moonbit_metadata(content: &str) -> Frontmatter {
    let mut fm = Frontmatter::default();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Some(comment) = line.strip_prefix("//") else {
            break;
        };
        let comment = comment.trim();
        let Some((key, value)) = comment.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key == "title" {
            fm.title = Some(strip_quotes(value));
        } else if key == "description" {
            fm.description = Some(strip_quotes(value));
        } else if key == "tags" {
            fm.tags = parse_tags(value);
        }
    }
    fm
}

fn parse_frontmatter(content: &str) -> (Frontmatter, &str) {
    let mut fm = Frontmatter::default();
    if !content.starts_with("---\n") {
        return (fm, content);
    }
    let rest = &content[4..];
    let Some(end_idx) = rest.find("\n---\n") else {
        return (fm, content);
    };
    let block = &rest[..end_idx];
    let body = &rest[end_idx + 5..];
    for raw in block.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if key == "title" {
            fm.title = Some(strip_quotes(value));
        } else if key == "description" {
            fm.description = Some(strip_quotes(value));
        } else if key == "tags" {
            fm.tags = parse_tags(value);
        }
    }
    (fm, body)
}

fn parse_tags(value: &str) -> Vec<String> {
    let v = strip_quotes(value);
    let trimmed = v.trim();
    let inner = if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    inner
        .split(',')
        .map(strip_quotes)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn strip_quotes(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        if (bytes[0] == b'"' && bytes[trimmed.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[trimmed.len() - 1] == b'\'')
        {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
}

fn extract_first_heading(body: &str) -> Option<String> {
    for raw in body.lines() {
        let line = raw.trim();
        if let Some(title) = line.strip_prefix("# ") {
            let title = title.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

fn extract_description(body: &str) -> Option<String> {
    let mut in_fence = false;
    for raw in body.lines() {
        let line = raw.trim();
        if line.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || line.is_empty() || line.starts_with('#') {
            continue;
        }
        return Some(line.to_string());
    }
    None
}

fn extract_first_shell_block(body: &str) -> Option<(String, String)> {
    let mut in_block = false;
    let mut capture = false;
    let mut shell = String::from("sh");
    let mut lines: Vec<String> = Vec::new();

    for raw in body.lines() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if in_block {
                if capture {
                    let command = lines.join("\n").trim().to_string();
                    if !command.is_empty() {
                        return Some((shell, command));
                    }
                }
                in_block = false;
                capture = false;
                lines.clear();
                continue;
            }

            in_block = true;
            let fence_info = trimmed.trim_start_matches("```").trim();
            let lang = fence_info
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if is_shell_lang(&lang) {
                capture = true;
                shell = normalize_shell_lang(&lang);
            } else {
                capture = false;
            }
            continue;
        }

        if in_block && capture {
            lines.push(line.to_string());
        }
    }
    None
}

fn is_shell_lang(lang: &str) -> bool {
    matches!(lang, "" | "sh" | "bash" | "zsh" | "shell" | "fish")
}

fn normalize_shell_lang(lang: &str) -> String {
    let normalized = lang.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized == "shell" {
        "sh".to_string()
    } else {
        normalized
    }
}

fn resolve_shell_bin(shell: &str) -> String {
    let token = shell.split_whitespace().next().unwrap_or("").trim();
    match token.to_ascii_lowercase().as_str() {
        "" | "sh" | "shell" => "/bin/sh".to_string(),
        "bash" => "bash".to_string(),
        "zsh" => "zsh".to_string(),
        "fish" => "fish".to_string(),
        other => {
            if other.is_empty() {
                "/bin/sh".to_string()
            } else {
                token.to_string()
            }
        }
    }
}

fn filter_recipes(recipes: Vec<Recipe>, query: Option<&str>) -> Vec<Recipe> {
    let Some(query) = query.map(|q| q.trim()).filter(|q| !q.is_empty()) else {
        return recipes;
    };
    let needle = query.to_ascii_lowercase();
    recipes
        .into_iter()
        .filter(|r| {
            let mut hay = String::new();
            hay.push_str(&r.id);
            hay.push(' ');
            hay.push_str(&r.name);
            hay.push(' ');
            hay.push_str(&r.description);
            if !r.tags.is_empty() {
                hay.push(' ');
                hay.push_str(&r.tags.join(" "));
            }
            hay.to_ascii_lowercase().contains(&needle)
        })
        .collect()
}

fn select_recipe<'a>(recipes: &'a [Recipe], selector: &str) -> Result<&'a Recipe> {
    let normalized = selector.trim();
    if normalized.is_empty() {
        bail!("empty recipe selector")
    }

    if let Some(recipe) = recipes.iter().find(|r| r.id == normalized) {
        return Ok(recipe);
    }

    let lowered = normalized.to_ascii_lowercase();
    let exact_name: Vec<&Recipe> = recipes
        .iter()
        .filter(|r| r.name.to_ascii_lowercase() == lowered)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0]);
    }
    if exact_name.len() > 1 {
        ambiguous_selector_error(selector, &exact_name)?;
        bail!("ambiguous recipe selector")
    }

    let contains: Vec<&Recipe> = recipes
        .iter()
        .filter(|r| {
            r.id.to_ascii_lowercase().contains(&lowered)
                || r.name.to_ascii_lowercase().contains(&lowered)
        })
        .collect();
    if contains.len() == 1 {
        return Ok(contains[0]);
    }
    if contains.is_empty() {
        bail!("no recipe matched '{}'", selector);
    }
    ambiguous_selector_error(selector, &contains)?;
    bail!("ambiguous recipe selector")
}

fn ambiguous_selector_error(selector: &str, matches: &[&Recipe]) -> Result<()> {
    eprintln!("recipe selector '{}' matched multiple recipes:", selector);
    for recipe in matches {
        eprintln!("  - {} ({})", recipe.id, recipe.name);
    }
    Ok(())
}

fn resolve_cwd(cwd: Option<&str>) -> Result<PathBuf> {
    if let Some(cwd) = cwd {
        return Ok(expand_tilde(cwd));
    }
    detect_project_root()
}

fn detect_project_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output();
    if let Ok(out) = output
        && out.status.success()
    {
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !root.is_empty() {
            return Ok(PathBuf::from(root));
        }
    }
    env::current_dir().context("failed to resolve current directory")
}

fn resolve_global_dir(project_root: &Path, override_dir: Option<&str>) -> PathBuf {
    let env_override = env::var(ENV_GLOBAL_RECIPE_DIR).ok();
    resolve_global_dir_with_env(project_root, override_dir, env_override.as_deref())
}

fn resolve_global_dir_with_env(
    _project_root: &Path,
    override_dir: Option<&str>,
    env_override: Option<&str>,
) -> PathBuf {
    if let Some(dir) = override_dir {
        return expand_tilde(dir);
    }
    if let Some(dir) = env_override
        && !dir.trim().is_empty()
    {
        return expand_tilde(dir);
    }
    config::global_config_dir().join("recipes")
}

fn expand_tilde(path: &str) -> PathBuf {
    PathBuf::from(tilde(path).to_string())
}

fn ensure_dir(dir: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    if dir.exists() {
        return Ok(());
    }
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    created.push(dir.to_path_buf());
    Ok(())
}

fn write_starter_recipe(path: &Path, content: &str, created: &mut Vec<PathBuf>) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    created.push(path.to_path_buf());
    Ok(())
}

const STARTER_PROJECT_RECIPE: &str = r#"---
title: Open Safari New Tab
description: Fast local smoke command for seq integration.
tags: [seq, app]
---

Open Safari via seq and create a new tab.

```sh
/Users/nikiv/code/seq/cli/cpp/out/bin/seq run "open Safari new tab"
```
"#;

const STARTER_PROJECT_BENCH_RECIPE: &str = r#"---
title: Kar User Command Bench
description: Run transport-focused bridge latency benchmark.
tags: [benchmark, latency, karabiner]
---

```sh
python3 tools/bridge_latency_bench.py --build-if-missing --iterations 300 --warmup 40
```
"#;

const STARTER_PROJECT_MOONBIT_RECIPE: &str = r#"// title: MoonBit Recipe Starter
// description: Minimal runnable MoonBit recipe entry.
// tags: [moonbit, recipe]

fn main {
  println("hello from moonbit recipe")
}
"#;

const STARTER_GLOBAL_RECIPE: &str = r#"---
title: System Ready Check
description: Verify machine is in clean state before latency benchmarks.
tags: [system, benchmark]
---

```sh
f kar-uc-system-check-report || true
```
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parses_basic_fields() {
        let text = "---\n\
title: Hello\n\
description: World\n\
tags: [a, b]\n\
---\n\
# Heading\n";
        let (fm, body) = parse_frontmatter(text);
        assert_eq!(fm.title.as_deref(), Some("Hello"));
        assert_eq!(fm.description.as_deref(), Some("World"));
        assert_eq!(fm.tags, vec!["a".to_string(), "b".to_string()]);
        assert!(body.starts_with("# Heading"));
    }

    #[test]
    fn extracts_shell_block() {
        let body = "# T\n\n```bash\necho hi\n```\n";
        let (shell, command) = extract_first_shell_block(body).expect("shell block");
        assert_eq!(shell, "bash");
        assert_eq!(command, "echo hi");
    }

    #[test]
    fn extracts_shell_block_with_fence_metadata() {
        let body = "# T\n\n```zsh title=\"run\"\necho hi\n```\n";
        let (shell, command) = extract_first_shell_block(body).expect("shell block");
        assert_eq!(shell, "zsh");
        assert_eq!(command, "echo hi");
    }

    #[test]
    fn normalize_shell_lang_handles_shell_alias() {
        assert_eq!(normalize_shell_lang("shell"), "sh");
        assert_eq!(normalize_shell_lang(""), "sh");
    }

    #[test]
    fn resolve_shell_bin_honors_declared_shell() {
        assert_eq!(resolve_shell_bin("bash"), "bash");
        assert_eq!(resolve_shell_bin("zsh"), "zsh");
        assert_eq!(resolve_shell_bin("fish"), "fish");
        assert_eq!(resolve_shell_bin("sh"), "/bin/sh");
        assert_eq!(resolve_shell_bin("shell"), "/bin/sh");
    }

    #[test]
    fn resolve_global_dir_prefers_override_then_env_then_config() {
        let root = PathBuf::from("/tmp/project");
        let override_dir = resolve_global_dir_with_env(&root, Some("~/recipes-x"), None);
        assert!(
            override_dir.to_string_lossy().contains("recipes-x"),
            "override dir should be used"
        );

        let env_dir = resolve_global_dir_with_env(&root, None, Some("~/recipes-y"));
        assert!(
            env_dir.to_string_lossy().contains("recipes-y"),
            "env dir should be used"
        );

        let cfg_dir = resolve_global_dir_with_env(&root, None, None);
        assert_eq!(cfg_dir, config::global_config_dir().join("recipes"));
    }

    #[test]
    fn filter_matches_name_and_tags() {
        let recipes = vec![Recipe {
            id: "project:a".to_string(),
            name: "Open Safari".to_string(),
            description: "fast".to_string(),
            path: PathBuf::from("a.md"),
            scope: Scope::Project,
            runner: RecipeRunner::Shell {
                shell: "sh".to_string(),
                command: "echo".to_string(),
            },
            tags: vec!["browser".to_string()],
        }];
        let out = filter_recipes(recipes, Some("browser"));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn parses_moonbit_metadata_header() {
        let text = "// title: Fast App Open\n\
// description: open app with moonbit\n\
// tags: [moonbit, fast]\n\
\n\
fn main {\n\
  println(\"ok\")\n\
}\n";
        let fm = parse_moonbit_metadata(text);
        assert_eq!(fm.title.as_deref(), Some("Fast App Open"));
        assert_eq!(fm.description.as_deref(), Some("open app with moonbit"));
        assert_eq!(fm.tags, vec!["moonbit".to_string(), "fast".to_string()]);
    }
}
