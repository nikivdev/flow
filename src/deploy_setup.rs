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
use regex::Regex;

use crate::env::parse_env_file;

#[derive(Debug, Clone, Default)]
pub struct CloudflareSetupDefaults {
    pub worker_path: Option<PathBuf>,
    pub env_file: Option<PathBuf>,
    pub environment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CloudflareSetupResult {
    pub worker_path: PathBuf,
    pub env_file: Option<PathBuf>,
    pub environment: Option<String>,
    pub selected_keys: Vec<String>,
    pub apply_secrets: bool,
}

pub fn run_cloudflare_setup(
    project_root: &Path,
    defaults: CloudflareSetupDefaults,
) -> Result<Option<CloudflareSetupResult>> {
    let worker_paths = discover_wrangler_configs(project_root)?;
    if worker_paths.is_empty() {
        println!("No Cloudflare Worker config found (wrangler.toml/json).");
        println!("Run `wrangler init` first, then try: f deploy setup");
        return Ok(None);
    }

    let env_files = discover_env_files(project_root)?;
    let mut app = DeploySetupApp::new(project_root, worker_paths, env_files, defaults);

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
    Worker,
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

struct DeploySetupApp {
    project_root: PathBuf,
    step: SetupStep,
    worker_paths: Vec<PathBuf>,
    selected_worker: usize,
    env_files: Vec<EnvFileChoice>,
    selected_env_file: usize,
    env_targets: Vec<EnvTargetChoice>,
    selected_env_target: usize,
    custom_env: String,
    key_items: Vec<EnvKeyItem>,
    selected_key: usize,
    apply_secrets: bool,
    result: Option<CloudflareSetupResult>,
}

impl DeploySetupApp {
    fn new(
        project_root: &Path,
        worker_paths: Vec<PathBuf>,
        env_files: Vec<PathBuf>,
        defaults: CloudflareSetupDefaults,
    ) -> Self {
        let selected_worker = pick_default_worker(&worker_paths, defaults.worker_path.as_ref());
        let env_file_choices = build_env_file_choices(project_root, &env_files);
        let selected_env_file = pick_default_env_file_for_worker(
            &env_file_choices,
            &worker_paths[selected_worker],
            defaults.env_file.as_ref(),
        );

        let mut app = Self {
            project_root: project_root.to_path_buf(),
            step: SetupStep::Worker,
            worker_paths,
            selected_worker,
            env_files: env_file_choices,
            selected_env_file,
            env_targets: Vec::new(),
            selected_env_target: 0,
            custom_env: String::new(),
            key_items: Vec::new(),
            selected_key: 0,
            apply_secrets: true,
            result: None,
        };

        app.refresh_env_targets(defaults.environment.as_deref());
        if matches!(
            app.env_targets.get(app.selected_env_target),
            Some(choice) if choice.is_custom
        ) {
            app.custom_env = defaults.environment.unwrap_or_default();
        }

        app
    }

    fn worker_path(&self) -> &Path {
        &self.worker_paths[self.selected_worker]
    }

    fn refresh_env_targets(&mut self, preferred: Option<&str>) {
        let envs = extract_wrangler_envs(self.worker_path());
        let mut targets = Vec::new();
        targets.push(EnvTargetChoice {
            label: "production (default)".to_string(),
            value: None,
            is_custom: false,
        });

        for env in envs {
            targets.push(EnvTargetChoice {
                label: env.clone(),
                value: Some(env),
                is_custom: false,
            });
        }

        if let Some(env) = preferred {
            if !targets.iter().any(|choice| choice.value.as_deref() == Some(env)) && env != "production" {
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

    fn select_env_file_for_worker(&mut self) {
        let worker_path = self.worker_path().to_path_buf();
        if let Some(idx) = pick_env_file_for_worker(&self.env_files, &worker_path) {
            self.selected_env_file = idx;
        }
    }

    fn refresh_keys(&mut self) {
        self.key_items.clear();
        self.selected_key = 0;

        if let Some(path) = self.env_files.get(self.selected_env_file).and_then(|c| c.path.clone())
        {
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
        let env_file = self.env_file_path();
        let mut selected_keys = Vec::new();
        if env_file.is_some() {
            selected_keys = self
                .key_items
                .iter()
                .filter(|item| item.selected)
                .map(|item| item.key.clone())
                .collect();
        }

        self.result = Some(CloudflareSetupResult {
            worker_path: self.worker_path().to_path_buf(),
            env_file,
            environment: self.selected_env_target(),
            selected_keys,
            apply_secrets: self.apply_secrets,
        });
    }
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut DeploySetupApp,
) -> Result<Option<CloudflareSetupResult>> {
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

fn handle_key(app: &mut DeploySetupApp, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Esc => return Ok(step_back(app)),
        _ => {}
    }

    match app.step {
        SetupStep::Worker => match key.code {
            KeyCode::Up => {
                select_prev(&mut app.selected_worker, app.worker_paths.len());
                app.select_env_file_for_worker();
            }
            KeyCode::Down => {
                select_next(&mut app.selected_worker, app.worker_paths.len());
                app.select_env_file_for_worker();
            }
            KeyCode::Enter => {
                app.refresh_env_targets(None);
                app.select_env_file_for_worker();
                if app.env_files.len() <= 1 {
                    app.step = SetupStep::EnvTarget;
                } else {
                    app.step = SetupStep::EnvFile;
                }
            }
            _ => {}
        },
        SetupStep::EnvFile => match key.code {
            KeyCode::Up => select_prev(&mut app.selected_env_file, app.env_files.len()),
            KeyCode::Down => select_next(&mut app.selected_env_file, app.env_files.len()),
            KeyCode::Enter => {
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
            KeyCode::Char(' ') => app.apply_secrets = !app.apply_secrets,
            KeyCode::Enter => {
                app.finalize();
                return Ok(true);
            }
            _ => {}
        },
    }

    Ok(false)
}

fn draw_ui(f: &mut ratatui::Frame<'_>, app: &DeploySetupApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1), Constraint::Length(3)].as_ref())
        .split(f.area());

    let title = match app.step {
        SetupStep::Worker => "Deploy Setup: Cloudflare Workers",
        SetupStep::EnvFile => "Select .env file (optional)",
        SetupStep::EnvTarget => "Select Cloudflare environment",
        SetupStep::CustomEnv => "Enter custom environment",
        SetupStep::Keys => "Select secrets to push",
        SetupStep::Confirm => "Confirm setup",
    };

    let header = Paragraph::new(Line::from(title))
        .block(Block::default().borders(Borders::ALL).title("flow"))
        .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(header, chunks[0]);

    match app.step {
        SetupStep::Worker => {
            let items = app
                .worker_paths
                .iter()
                .map(|path| {
                    let label = relative_display(&app.project_root, path);
                    ListItem::new(Line::from(label))
                })
                .collect::<Vec<_>>();

            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title("Worker path"))
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
            let mut state = ratatui::widgets::ListState::default();
            state.select(Some(app.selected_worker));
            f.render_stateful_widget(list, chunks[1], &mut state);
        }
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
                .block(Block::default().borders(Borders::ALL).title("Secrets source"))
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
                .block(Block::default().borders(Borders::ALL).title("Wrangler --env"))
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
                .block(Block::default().borders(Borders::ALL).title("Environment name"))
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
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(
                            "Secrets ({}/{})",
                            selected_count,
                            app.key_items.len()
                        )),
                )
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
            let worker = relative_display(&app.project_root, app.worker_path());
            let env_file = app
                .env_file_path()
                .map(|p| relative_display(&app.project_root, &p))
                .unwrap_or_else(|| "none".to_string());
            let env_target = app
                .selected_env_target()
                .unwrap_or_else(|| "production (default)".to_string());
            let selected_count = app.key_items.iter().filter(|item| item.selected).count();
            let apply = if app.apply_secrets { "yes" } else { "no" };
            let summary = vec![
                Line::from(vec![
                    Span::styled("Worker: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(worker),
                ]),
                Line::from(vec![
                    Span::styled("Env file: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(env_file),
                ]),
                Line::from(vec![
                    Span::styled("Environment: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(env_target),
                ]),
                Line::from(vec![
                    Span::styled("Secrets selected: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(format!("{}", selected_count)),
                ]),
                Line::from(vec![
                    Span::styled("Apply secrets now: ", Style::default().add_modifier(Modifier::BOLD)),
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
        SetupStep::Worker => "Up/Down to move, Enter to select, Esc to cancel, q to cancel",
        SetupStep::EnvFile => "Up/Down to move, Enter to select, Esc to back, q to cancel",
        SetupStep::EnvTarget => "Up/Down to move, Enter to select, Esc to back, q to cancel",
        SetupStep::CustomEnv => "Type name, Enter to confirm, Esc to back, q to cancel",
        SetupStep::Keys => "Up/Down to move, Space to toggle, Enter to continue, Esc to back, q to cancel",
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

fn step_back(app: &mut DeploySetupApp) -> bool {
    match app.step {
        SetupStep::Worker => true,
        SetupStep::EnvFile => {
            app.step = SetupStep::Worker;
            false
        }
        SetupStep::EnvTarget => {
            if app.env_files.len() <= 1 {
                app.step = SetupStep::Worker;
            } else {
                app.step = SetupStep::EnvFile;
            }
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
        if rel.is_empty() {
            ".".to_string()
        } else {
            rel
        }
    } else {
        path.to_string_lossy().to_string()
    }
}

fn pick_default_worker(paths: &[PathBuf], preferred: Option<&PathBuf>) -> usize {
    if let Some(path) = preferred {
        if let Some((idx, _)) = paths.iter().enumerate().find(|(_, p)| *p == path) {
            return idx;
        }
    }
    0
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

fn pick_default_env_file_for_worker(
    choices: &[EnvFileChoice],
    worker_path: &Path,
    preferred: Option<&PathBuf>,
) -> usize {
    if let Some(path) = preferred {
        if let Some((idx, _)) = choices.iter().enumerate().find(|(_, c)| c.path.as_ref() == Some(path)) {
            return idx;
        }
    }

    if let Some(idx) = pick_env_file_for_worker(choices, worker_path) {
        return idx;
    }

    0
}

fn pick_env_file_for_worker(choices: &[EnvFileChoice], worker_path: &Path) -> Option<usize> {
    let candidates = [
        ".env",
        ".env.cloudflare",
        ".env.production",
        ".env.staging",
        ".env.local",
    ];

    for candidate in candidates {
        let candidate_path = worker_path.join(candidate);
        if let Some((idx, _)) = choices
            .iter()
            .enumerate()
            .find(|(_, c)| c.path.as_ref() == Some(&candidate_path))
        {
            return Some(idx);
        }
    }

    None
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

pub(crate) fn discover_wrangler_configs(root: &Path) -> Result<Vec<PathBuf>> {
    let walker = WalkBuilder::new(root)
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

    let mut paths = Vec::new();
    for entry in walker.flatten() {
        if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            if let Some(name) = entry.path().file_name().and_then(|s| s.to_str()) {
                if matches!(name, "wrangler.toml" | "wrangler.json" | "wrangler.jsonc") {
                    if let Some(parent) = entry.path().parent() {
                        paths.push(parent.to_path_buf());
                    }
                }
            }
        }
    }

    paths.sort();
    paths.dedup();
    Ok(paths)
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

fn extract_wrangler_envs(worker_path: &Path) -> Vec<String> {
    let toml_path = worker_path.join("wrangler.toml");
    if toml_path.exists() {
        if let Ok(content) = fs::read_to_string(&toml_path) {
            let re = Regex::new(r"^\s*\[env\.([^\]]+)\]\s*$").unwrap();
            let mut envs = Vec::new();
            for line in content.lines() {
                if let Some(caps) = re.captures(line) {
                    let env = caps.get(1).map(|m| m.as_str().trim().to_string());
                    if let Some(env) = env {
                        if !env.is_empty() {
                            envs.push(env);
                        }
                    }
                }
            }
            envs.sort();
            envs.dedup();
            return envs;
        }
    }

    Vec::new()
}
