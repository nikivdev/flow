use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event as CEvent, KeyCode, KeyEvent},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ignore::WalkBuilder;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

use crate::env::parse_env_file;

#[derive(Debug, Clone, Default)]
pub struct EnvSetupDefaults {
    pub env_file: Option<PathBuf>,
    pub environment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EnvSetupResult {
    pub env_file: Option<PathBuf>,
    pub environment: String,
    pub selected_keys: Vec<String>,
    pub apply: bool,
}

pub fn run_env_setup(
    project_root: &Path,
    defaults: EnvSetupDefaults,
) -> Result<Option<EnvSetupResult>> {
    let env_files = discover_env_files(project_root)?;
    if env_files.is_empty() {
        println!("No .env files found.");
        println!("Create one (for example .env) and try: f env setup");
        return Ok(None);
    }

    let mut app = EnvSetupApp::new(project_root, env_files, defaults);

    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;

    let app_result = run_app(&mut terminal, &mut app);

    disable_raw_mode().ok();
    let _ = terminal.show_cursor();
    drop(terminal);
    let mut stdout = std::io::stdout();
    execute!(stdout, LeaveAlternateScreen).ok();

    app_result
}

#[derive(Debug, Clone, Copy)]
enum SetupStep {
    EnvFile,
    EnvTarget,
    CustomEnv,
    Keys,
    Confirm,
}

struct EnvFileChoice {
    label: String,
    path: Option<PathBuf>,
}

struct EnvTargetChoice {
    label: String,
    value: Option<String>,
    is_custom: bool,
}

struct EnvKeyItem {
    key: String,
    selected: bool,
    suspect: bool,
    suspect_reason: Option<String>,
    value_len: usize,
}

struct EnvSetupApp {
    project_root: PathBuf,
    step: SetupStep,
    env_files: Vec<EnvFileChoice>,
    selected_env_file: usize,
    env_targets: Vec<EnvTargetChoice>,
    selected_env_target: usize,
    custom_env: String,
    key_items: Vec<EnvKeyItem>,
    selected_key: usize,
    apply: bool,
    result: Option<EnvSetupResult>,
}

impl EnvSetupApp {
    fn new(project_root: &Path, env_files: Vec<PathBuf>, defaults: EnvSetupDefaults) -> Self {
        let env_file_choices = build_env_file_choices(project_root, &env_files);
        let selected_env_file =
            pick_default_env_file(project_root, &env_file_choices, defaults.env_file.as_ref());

        let mut app = Self {
            project_root: project_root.to_path_buf(),
            step: SetupStep::EnvFile,
            env_files: env_file_choices,
            selected_env_file,
            env_targets: Vec::new(),
            selected_env_target: 0,
            custom_env: String::new(),
            key_items: Vec::new(),
            selected_key: 0,
            apply: true,
            result: None,
        };

        let preferred = defaults
            .environment
            .as_deref()
            .map(|s| s.to_string())
            .or_else(|| app.infer_env_target());
        app.refresh_env_targets(preferred.as_deref());

        app
    }

    fn infer_env_target(&self) -> Option<String> {
        let path = self.env_file_path()?;
        infer_env_target_from_file(&path)
    }

    fn refresh_env_targets(&mut self, preferred: Option<&str>) {
        let mut targets = vec![
            EnvTargetChoice {
                label: "production (default)".to_string(),
                value: Some("production".to_string()),
                is_custom: false,
            },
            EnvTargetChoice {
                label: "staging".to_string(),
                value: Some("staging".to_string()),
                is_custom: false,
            },
            EnvTargetChoice {
                label: "dev".to_string(),
                value: Some("dev".to_string()),
                is_custom: false,
            },
        ];

        if let Some(env) = preferred {
            if !targets
                .iter()
                .any(|choice| choice.value.as_deref() == Some(env))
            {
                targets.push(EnvTargetChoice {
                    label: env.to_string(),
                    value: Some(env.to_string()),
                    is_custom: false,
                });
            }
        }

        targets.push(EnvTargetChoice {
            label: "custom...".to_string(),
            value: None,
            is_custom: true,
        });

        self.env_targets = targets;
        self.selected_env_target = pick_default_env_target(&self.env_targets, preferred);
    }

    fn refresh_keys(&mut self) {
        self.key_items.clear();
        self.selected_key = 0;

        if let Some(path) = self.env_file_path() {
            if let Ok(items) = build_key_items(&path) {
                self.key_items = items;
            }
        }
    }

    fn env_file_path(&self) -> Option<PathBuf> {
        self.env_files
            .get(self.selected_env_file)
            .and_then(|choice| choice.path.clone())
    }

    fn env_file_path_ref(&self) -> Option<&Path> {
        self.env_files
            .get(self.selected_env_file)
            .and_then(|choice| choice.path.as_deref())
    }

    fn selected_env_target(&self) -> Option<String> {
        self.env_targets
            .get(self.selected_env_target)
            .and_then(|choice| choice.value.clone())
    }

    fn finalize(&mut self) {
        let selected_keys = self
            .key_items
            .iter()
            .filter(|item| item.selected)
            .map(|item| item.key.clone())
            .collect();

        let environment = self
            .selected_env_target()
            .unwrap_or_else(|| "production".to_string());

        self.result = Some(EnvSetupResult {
            env_file: self.env_file_path(),
            environment,
            selected_keys,
            apply: self.apply,
        });
    }
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut EnvSetupApp,
) -> Result<Option<EnvSetupResult>> {
    loop {
        terminal.draw(|f| draw_ui(f, app))?;

        if event::poll(std::time::Duration::from_millis(200))? {
            if let CEvent::Key(key) = event::read()? {
                if handle_key(app, key)? {
                    return Ok(app.result.take());
                }
            }
        }
    }
}

fn handle_key(app: &mut EnvSetupApp, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Esc => return Ok(step_back(app)),
        _ => {}
    }

    match app.step {
        SetupStep::EnvFile => match key.code {
            KeyCode::Up => {
                select_prev(&mut app.selected_env_file, app.env_files.len());
                let preferred = app.infer_env_target();
                app.refresh_env_targets(preferred.as_deref());
            }
            KeyCode::Down => {
                select_next(&mut app.selected_env_file, app.env_files.len());
                let preferred = app.infer_env_target();
                app.refresh_env_targets(preferred.as_deref());
            }
            KeyCode::Enter => {
                let preferred = app.infer_env_target();
                app.refresh_env_targets(preferred.as_deref());
                app.step = SetupStep::EnvTarget;
            }
            _ => {}
        },
        SetupStep::EnvTarget => match key.code {
            KeyCode::Up => select_prev(&mut app.selected_env_target, app.env_targets.len()),
            KeyCode::Down => select_next(&mut app.selected_env_target, app.env_targets.len()),
            KeyCode::Enter => {
                if app
                    .env_targets
                    .get(app.selected_env_target)
                    .is_some_and(|choice| choice.is_custom)
                {
                    app.custom_env.clear();
                    app.step = SetupStep::CustomEnv;
                } else if app.env_file_path().is_some() {
                    app.refresh_keys();
                    if app.key_items.is_empty() {
                        app.step = SetupStep::Confirm;
                    } else {
                        app.step = SetupStep::Keys;
                    }
                } else {
                    app.step = SetupStep::Confirm;
                }
            }
            _ => {}
        },
        SetupStep::CustomEnv => match key.code {
            KeyCode::Enter => {
                if !app.custom_env.trim().is_empty() {
                    app.env_targets.push(EnvTargetChoice {
                        label: app.custom_env.trim().to_string(),
                        value: Some(app.custom_env.trim().to_string()),
                        is_custom: false,
                    });
                    app.selected_env_target = app.env_targets.len().saturating_sub(2);
                    if app.env_file_path().is_some() {
                        app.refresh_keys();
                        app.step = if app.key_items.is_empty() {
                            SetupStep::Confirm
                        } else {
                            SetupStep::Keys
                        };
                    } else {
                        app.step = SetupStep::Confirm;
                    }
                }
            }
            KeyCode::Backspace => {
                app.custom_env.pop();
            }
            KeyCode::Char(ch) => {
                if !ch.is_control() {
                    app.custom_env.push(ch);
                }
            }
            _ => {}
        },
        SetupStep::Keys => match key.code {
            KeyCode::Up => select_prev(&mut app.selected_key, app.key_items.len()),
            KeyCode::Down => select_next(&mut app.selected_key, app.key_items.len()),
            KeyCode::Char(' ') => {
                if let Some(item) = app.key_items.get_mut(app.selected_key) {
                    item.selected = !item.selected;
                }
            }
            KeyCode::Enter => app.step = SetupStep::Confirm,
            _ => {}
        },
        SetupStep::Confirm => match key.code {
            KeyCode::Char(' ') => app.apply = !app.apply,
            KeyCode::Enter => {
                app.finalize();
                return Ok(true);
            }
            _ => {}
        },
    }

    Ok(false)
}

fn draw_ui(f: &mut ratatui::Frame<'_>, app: &EnvSetupApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            [
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(3),
            ]
            .as_ref(),
        )
        .split(f.area());

    let title = match app.step {
        SetupStep::EnvFile => "Env Setup: Select .env file",
        SetupStep::EnvTarget => "Select cloud environment",
        SetupStep::CustomEnv => "Enter custom environment",
        SetupStep::Keys => "Select keys to push",
        SetupStep::Confirm => "Confirm env setup",
    };

    let header = Paragraph::new(Line::from(title))
        .block(Block::default().borders(Borders::ALL).title("flow"))
        .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(header, chunks[0]);

    match app.step {
        SetupStep::EnvFile => {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)].as_ref())
                .split(chunks[1]);
            let items = app
                .env_files
                .iter()
                .map(|choice| ListItem::new(Line::from(choice.label.clone())))
                .collect::<Vec<_>>();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Secrets source"),
                )
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
            let mut state = ratatui::widgets::ListState::default();
            state.select(Some(app.selected_env_file));
            f.render_stateful_widget(list, body[0], &mut state);

            let preview_lines = build_env_preview_lines(&app.project_root, app.env_file_path_ref());
            let preview = Paragraph::new(preview_lines)
                .block(Block::default().borders(Borders::ALL).title("Preview"))
                .wrap(Wrap { trim: true });
            f.render_widget(preview, body[1]);
        }
        SetupStep::EnvTarget => {
            let items = app
                .env_targets
                .iter()
                .map(|choice| ListItem::new(Line::from(choice.label.clone())))
                .collect::<Vec<_>>();
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title("Environment"))
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
            let mut state = ratatui::widgets::ListState::default();
            state.select(Some(app.selected_env_target));
            f.render_stateful_widget(list, chunks[1], &mut state);
        }
        SetupStep::CustomEnv => {
            let prompt = format!("> {}", app.custom_env);
            let input = Paragraph::new(prompt)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Environment name"),
                )
                .wrap(Wrap { trim: true });
            f.render_widget(input, chunks[1]);
        }
        SetupStep::Keys => {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(60), Constraint::Percentage(40)].as_ref())
                .split(chunks[1]);
            let selected_count = app.key_items.iter().filter(|item| item.selected).count();
            let items = app
                .key_items
                .iter()
                .map(|item| {
                    let indicator = if item.selected { "[x]" } else { "[ ]" };
                    let flag = if item.suspect { "  suspect" } else { "" };
                    let label = format!("{indicator} {}{flag}", item.key);
                    ListItem::new(Line::from(label))
                })
                .collect::<Vec<_>>();
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "Keys ({}/{})",
                    selected_count,
                    app.key_items.len()
                )))
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
            let mut state = ratatui::widgets::ListState::default();
            state.select(Some(app.selected_key));
            f.render_stateful_widget(list, body[0], &mut state);

            let detail_lines = build_key_detail_lines(
                &app.project_root,
                app.env_file_path_ref(),
                app.key_items.get(app.selected_key),
            );
            let details = Paragraph::new(detail_lines)
                .block(Block::default().borders(Borders::ALL).title("Details"))
                .wrap(Wrap { trim: true });
            f.render_widget(details, body[1]);
        }
        SetupStep::Confirm => {
            let env_file = app
                .env_file_path()
                .map(|p| relative_display(&app.project_root, &p))
                .unwrap_or_else(|| "none".to_string());
            let env_target = app
                .selected_env_target()
                .unwrap_or_else(|| "production".to_string());
            let selected_count = app.key_items.iter().filter(|item| item.selected).count();
            let apply = if app.apply { "yes" } else { "no" };
            let summary = vec![
                Line::from(vec![
                    Span::styled("Env file: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(env_file),
                ]),
                Line::from(vec![
                    Span::styled(
                        "Environment: ",
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(env_target),
                ]),
                Line::from(vec![
                    Span::styled(
                        "Keys selected: ",
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("{}", selected_count)),
                ]),
                Line::from(vec![
                    Span::styled("Apply now: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(apply),
                ]),
            ];

            let paragraph = Paragraph::new(summary)
                .block(Block::default().borders(Borders::ALL).title("Review"))
                .wrap(Wrap { trim: true });
            f.render_widget(paragraph, chunks[1]);
        }
    }

    let help = match app.step {
        SetupStep::EnvFile => "Up/Down to move, Enter to select, Esc to cancel, q to cancel",
        SetupStep::EnvTarget => "Up/Down to move, Enter to select, Esc to back, q to cancel",
        SetupStep::CustomEnv => "Type name, Enter to confirm, Esc to back, q to cancel",
        SetupStep::Keys => {
            "Up/Down to move, Space to toggle, Enter to continue, Esc to back, q to cancel"
        }
        SetupStep::Confirm => "Space to toggle apply, Enter to finish, Esc to back, q to cancel",
    };
    let footer = Paragraph::new(help)
        .block(Block::default().borders(Borders::ALL))
        .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(footer, chunks[2]);
}

fn build_env_preview_lines(project_root: &Path, env_file: Option<&Path>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let Some(path) = env_file else {
        lines.push(Line::from("No env file selected."));
        lines.push(Line::from("Secrets will not be set."));
        return lines;
    };

    lines.push(Line::from(vec![
        Span::styled("File: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(relative_display(project_root, path)),
    ]));
    lines.push(Line::from("Values are hidden."));

    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => {
            lines.push(Line::from("Unable to read file."));
            return lines;
        }
    };

    let vars = parse_env_file(&content);
    if vars.is_empty() {
        lines.push(Line::from("No env vars found."));
        return lines;
    }

    let mut entries: Vec<_> = vars.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let suspect_count = entries
        .iter()
        .filter(|(_, value)| suspect_reason(value).is_some())
        .count();
    let total = entries.len();

    lines.push(Line::from(format!(
        "Keys: {} (suspect: {})",
        total, suspect_count
    )));
    lines.push(Line::from("! = likely test/local value"));

    let max_keys = 12usize;
    for (key, value) in entries.iter().take(max_keys) {
        let flag = if suspect_reason(value).is_some() {
            " !"
        } else {
            ""
        };
        lines.push(Line::from(format!(" - {}{}", key, flag)));
    }

    if total > max_keys {
        lines.push(Line::from(format!("... +{} more", total - max_keys)));
    }

    lines
}

fn build_key_detail_lines(
    project_root: &Path,
    env_file: Option<&Path>,
    item: Option<&EnvKeyItem>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let env_label = env_file
        .map(|path| relative_display(project_root, path))
        .unwrap_or_else(|| "none".to_string());
    lines.push(Line::from(format!("Env file: {}", env_label)));

    let Some(item) = item else {
        lines.push(Line::from("No key selected."));
        return lines;
    };

    lines.push(Line::from(format!("Key: {}", item.key)));
    lines.push(Line::from(format!(
        "Selected: {}",
        if item.selected { "yes" } else { "no" }
    )));
    lines.push(Line::from(format!(
        "Status: {}",
        if item.suspect { "suspect" } else { "ok" }
    )));
    if let Some(reason) = &item.suspect_reason {
        lines.push(Line::from(format!("Reason: {}", reason)));
    }
    lines.push(Line::from(format!("Value length: {}", item.value_len)));
    lines.push(Line::from("Values are hidden."));
    if item.suspect {
        lines.push(Line::from("Tip: suspect values default to unchecked."));
    }

    lines
}

fn select_prev(selected: &mut usize, len: usize) {
    if len == 0 {
        return;
    }
    if *selected == 0 {
        *selected = len.saturating_sub(1);
    } else {
        *selected -= 1;
    }
}

fn select_next(selected: &mut usize, len: usize) {
    if len == 0 {
        return;
    }
    if *selected + 1 >= len {
        *selected = 0;
    } else {
        *selected += 1;
    }
}

fn step_back(app: &mut EnvSetupApp) -> bool {
    match app.step {
        SetupStep::EnvFile => true,
        SetupStep::EnvTarget => {
            app.step = SetupStep::EnvFile;
            false
        }
        SetupStep::CustomEnv => {
            app.step = SetupStep::EnvTarget;
            false
        }
        SetupStep::Keys => {
            app.step = SetupStep::EnvTarget;
            false
        }
        SetupStep::Confirm => {
            if app.env_file_path().is_some() && !app.key_items.is_empty() {
                app.step = SetupStep::Keys;
            } else {
                app.step = SetupStep::EnvTarget;
            }
            false
        }
    }
}

fn relative_display(root: &Path, path: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(root) {
        let rel = rel.to_string_lossy().to_string();
        if rel.is_empty() { ".".to_string() } else { rel }
    } else {
        path.to_string_lossy().to_string()
    }
}

fn build_env_file_choices(project_root: &Path, env_files: &[PathBuf]) -> Vec<EnvFileChoice> {
    let mut choices = Vec::new();
    choices.push(EnvFileChoice {
        label: "Skip (do not set secrets)".to_string(),
        path: None,
    });

    for path in env_files {
        choices.push(EnvFileChoice {
            label: relative_display(project_root, path),
            path: Some(path.clone()),
        });
    }

    choices
}

fn pick_default_env_file(
    project_root: &Path,
    choices: &[EnvFileChoice],
    preferred: Option<&PathBuf>,
) -> usize {
    if let Some(path) = preferred {
        if let Some((idx, _)) = choices
            .iter()
            .enumerate()
            .find(|(_, c)| c.path.as_ref() == Some(path))
        {
            return idx;
        }
    }

    let candidates = [
        ".env",
        ".env.production",
        ".env.staging",
        ".env.dev",
        ".env.local",
    ];
    for candidate in candidates {
        let candidate_path = project_root.join(candidate);
        if let Some((idx, _)) = choices
            .iter()
            .enumerate()
            .find(|(_, c)| c.path.as_ref() == Some(&candidate_path))
        {
            return idx;
        }
    }

    if choices.len() > 1 { 1 } else { 0 }
}

fn pick_default_env_target(targets: &[EnvTargetChoice], preferred: Option<&str>) -> usize {
    if let Some(env) = preferred {
        if let Some((idx, _)) = targets
            .iter()
            .enumerate()
            .find(|(_, choice)| choice.value.as_deref() == Some(env))
        {
            return idx;
        }
    }
    0
}

fn build_key_items(path: &Path) -> Result<Vec<EnvKeyItem>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read env file {}", path.display()))?;
    let env = parse_env_file(&content);
    let mut keys: Vec<_> = env.into_iter().collect();
    keys.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(keys
        .into_iter()
        .map(|(key, value)| {
            let reason = suspect_reason(&value);
            let suspect = reason.is_some();
            EnvKeyItem {
                key,
                selected: !suspect,
                suspect: suspect || value.trim().is_empty(),
                suspect_reason: reason.map(|reason| reason.to_string()),
                value_len: value.len(),
            }
        })
        .collect())
}

fn suspect_reason(value: &str) -> Option<&'static str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Some("empty");
    }

    let lowered = trimmed.to_lowercase();
    if lowered.contains("sk_test") || lowered.contains("pk_test") {
        return Some("stripe_test");
    }
    if lowered.contains("localhost") || lowered.contains("127.0.0.1") {
        return Some("localhost");
    }
    if lowered.contains("example.com") || lowered.contains("example") {
        return Some("example");
    }
    if lowered.contains("dummy") {
        return Some("dummy");
    }
    if lowered.contains("test") {
        return Some("test");
    }

    None
}

fn infer_env_target_from_file(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    if name.contains("staging") {
        return Some("staging".to_string());
    }
    if name.contains("dev") || name.contains("development") {
        return Some("dev".to_string());
    }
    if name.contains("prod") || name.contains("production") {
        return Some("production".to_string());
    }
    None
}

fn discover_env_files(root: &Path) -> Result<Vec<PathBuf>> {
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
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

    let mut env_files = Vec::new();
    for entry in walker.flatten() {
        if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            if let Some(name) = entry.path().file_name().and_then(|s| s.to_str()) {
                if name.starts_with(".env") && name != ".envrc" {
                    env_files.push(entry.path().to_path_buf());
                }
            }
        }
    }

    env_files.sort();
    env_files.dedup();
    Ok(env_files)
}
