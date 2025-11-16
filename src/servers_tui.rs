use std::{
    io,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event as CEvent, KeyCode, KeyEvent},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    cli::ServersOpts,
    servers::{LogLine, LogStream, ServerSnapshot},
};

const LOG_LIMIT: usize = 512;

pub fn run(opts: ServersOpts) -> Result<()> {
    let base_url = format!("http://{}:{}", opts.host, opts.port);
    let client = reqwest::blocking::Client::new();

    // Set up terminal
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;

    let app_result = run_app(&mut terminal, client, base_url);

    // Restore terminal state on exit
    disable_raw_mode().ok();
    let _ = terminal.show_cursor();
    drop(terminal);
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen).ok();

    app_result
}

struct App {
    client: reqwest::blocking::Client,
    base_url: String,
    servers: Vec<ServerSnapshot>,
    selected: usize,
    logs: Vec<LogLine>,
    log_scroll: u16,
    focus_server: bool,
    last_servers_refresh: Instant,
    last_logs_refresh: Instant,
}

impl App {
    fn new(client: reqwest::blocking::Client, base_url: String) -> Result<Self> {
        let mut app = Self {
            client,
            base_url,
            servers: Vec::new(),
            selected: 0,
            logs: Vec::new(),
            log_scroll: 0,
            focus_server: false,
            last_servers_refresh: Instant::now(),
            last_logs_refresh: Instant::now(),
        };
        app.refresh_servers()?;
        app.refresh_logs()?;
        Ok(app)
    }

    fn selected_server_name(&self) -> Option<&str> {
        self.servers.get(self.selected).map(|s| s.name.as_str())
    }

    fn refresh_servers(&mut self) -> Result<()> {
        let url = format!("{}/servers", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .with_context(|| format!("failed to GET {url}"))?;

        if !resp.status().is_success() {
            anyhow::bail!(
                "daemon responded with {} when fetching servers",
                resp.status()
            );
        }

        let servers = resp
            .json::<Vec<ServerSnapshot>>()
            .context("failed to decode /servers response")?;

        self.servers = servers;
        if self.selected >= self.servers.len() {
            self.selected = self.servers.len().saturating_sub(1);
        }
        self.last_servers_refresh = Instant::now();
        Ok(())
    }

    fn refresh_logs(&mut self) -> Result<()> {
        let request = if self.focus_server {
            let name = match self.selected_server_name() {
                Some(name) => name,
                None => {
                    self.logs.clear();
                    self.focus_server = false;
                    self.last_logs_refresh = Instant::now();
                    return Ok(());
                }
            };
            format!("{}/servers/{}/logs", self.base_url, name)
        } else {
            format!("{}/logs", self.base_url)
        };

        let resp = self
            .client
            .get(&request)
            .query(&[("limit", LOG_LIMIT)])
            .send()
            .with_context(|| format!("failed to GET {request}"))?;

        if resp.status().is_success() {
            let logs = resp
                .json::<Vec<LogLine>>()
                .context("failed to decode logs response")?;
            self.logs = logs;
        } else {
            self.logs.clear();
        }

        self.last_logs_refresh = Instant::now();
        Ok(())
    }

    fn maybe_refresh(&mut self) -> Result<()> {
        let now = Instant::now();
        if now.duration_since(self.last_servers_refresh) > Duration::from_secs(5) {
            let _ = self.refresh_servers();
        }
        if now.duration_since(self.last_logs_refresh) > Duration::from_secs(1) {
            let _ = self.refresh_logs();
        }
        Ok(())
    }

    fn select_next(&mut self) -> Result<()> {
        if !self.servers.is_empty() && self.selected + 1 < self.servers.len() {
            self.selected += 1;
            self.log_scroll = 0;
            self.refresh_logs()?;
        }
        Ok(())
    }

    fn select_prev(&mut self) -> Result<()> {
        if !self.servers.is_empty() && self.selected > 0 {
            self.selected -= 1;
            self.log_scroll = 0;
            self.refresh_logs()?;
        }
        Ok(())
    }

    fn scroll_down(&mut self) {
        self.log_scroll = self.log_scroll.saturating_add(1);
    }

    fn scroll_up(&mut self) {
        self.log_scroll = self.log_scroll.saturating_sub(1);
    }

    fn toggle_focus(&mut self) -> Result<()> {
        if self.servers.is_empty() {
            return Ok(());
        }
        self.focus_server = !self.focus_server;
        self.log_scroll = 0;
        self.refresh_logs()
    }

    fn show_all_logs(&mut self) -> Result<()> {
        if self.focus_server {
            self.focus_server = false;
            self.log_scroll = 0;
            self.refresh_logs()
        } else {
            Ok(())
        }
    }

    fn log_scope_label(&self) -> String {
        if self.focus_server {
            match self.selected_server_name() {
                Some(name) => format!("Focused: {}", name),
                None => "Focused: (none)".to_string(),
            }
        } else {
            "All servers".to_string()
        }
    }
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    client: reqwest::blocking::Client,
    base_url: String,
) -> Result<()> {
    let mut app = App::new(client, base_url)?;
    let tick_rate = Duration::from_millis(250);

    loop {
        terminal
            .draw(|f| draw_ui(f, &app))
            .context("failed to draw TUI frame")?;

        if crossterm::event::poll(tick_rate)? {
            if let CEvent::Key(key) = event::read()? {
                if handle_key(&mut app, key)? {
                    break;
                }
            }
        }

        app.maybe_refresh()?;
    }

    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Esc => return Ok(true),
        KeyCode::Down | KeyCode::Char('j') => {
            app.select_next()?;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.select_prev()?;
        }
        KeyCode::PageDown | KeyCode::Char('J') => {
            app.scroll_down();
        }
        KeyCode::PageUp | KeyCode::Char('K') => {
            app.scroll_up();
        }
        KeyCode::Char('r') => {
            app.refresh_servers()?;
            app.refresh_logs()?;
        }
        KeyCode::Char('f') => {
            app.toggle_focus()?;
        }
        KeyCode::Char('a') => {
            app.show_all_logs()?;
        }
        _ => {}
    }

    Ok(false)
}

fn draw_ui(f: &mut ratatui::Frame<'_>, app: &App) {
    let size = f.size();

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(size);

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(layout[0]);

    // Servers list
    let servers_items: Vec<ListItem> = if app.servers.is_empty() {
        vec![ListItem::new("No servers (check config or daemon)")]
    } else {
        app.servers
            .iter()
            .map(|s| {
                let label = format!("{} [{}]", s.name, s.status);
                ListItem::new(label)
            })
            .collect()
    };

    let mut list_state = ListState::default();
    if !app.servers.is_empty() {
        list_state.select(Some(app.selected));
    }

    let servers_list = List::new(servers_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Servers (↑/↓, r = reload, q = quit)"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(servers_list, chunks[0], &mut list_state);

    // Logs pane
    let log_lines: Vec<Line> = if app.logs.is_empty() {
        vec![Line::from(Span::raw("No logs yet"))]
    } else {
        app.logs
            .iter()
            .map(|line| {
                let ts = format_ts(line.timestamp_ms);
                let stream = match line.stream {
                    LogStream::Stdout => ("OUT", Style::default().fg(Color::Green)),
                    LogStream::Stderr => ("ERR", Style::default().fg(Color::Red)),
                };
                let server_label = Span::styled(
                    format!("{:<12}", line.server),
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                );
                Line::from(vec![
                    Span::styled(
                        format!("[{ts}]"),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    Span::raw(" "),
                    server_label,
                    Span::raw(" "),
                    Span::styled(stream.0, stream.1.add_modifier(Modifier::BOLD)),
                    Span::raw(" "),
                    Span::raw(line.line.trim_end()),
                ])
            })
            .collect()
    };

    let scope = app.log_scope_label();
    let title = format!("Logs ({scope}) – PgUp/PgDn scroll • f focus toggle • a all logs");

    let logs_widget = Paragraph::new(log_lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((app.log_scroll, 0));

    f.render_widget(logs_widget, chunks[1]);

    let help = Paragraph::new(Line::from(vec![
        Span::styled("Hub: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(&app.base_url),
        Span::raw("  |  q quit • r refresh • j/k select • f focus • a all logs"),
    ]))
    .block(Block::default().borders(Borders::ALL).title("Help"))
    .wrap(Wrap { trim: true });

    f.render_widget(help, layout[1]);
}

fn format_ts(ms: u128) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    format!("{secs}.{millis:03}")
}
