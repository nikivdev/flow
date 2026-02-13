use std::io::{self, IsTerminal};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use opentui_lite::{ATTR_BOLD, BORDER_SIMPLE, Color, OpenTui};

pub fn confirm(title: &str, lines: &[String], default_yes: bool) -> Option<bool> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return None;
    }

    let (width, height) = crossterm::terminal::size().ok()?;
    let opentui = OpenTui::load().ok()?;
    let renderer = opentui
        .create_renderer(width as u32, height as u32, false)
        .ok()?;

    renderer.setup_terminal(true);

    let _raw = RawModeGuard::new().ok()?;

    let bg = Color::rgb(0.06, 0.07, 0.09);
    let border = Color::rgb(0.32, 0.42, 0.62);
    let text = Color::rgb(0.92, 0.94, 0.96);
    let muted = Color::rgb(0.68, 0.72, 0.78);
    let accent = Color::rgb(0.90, 0.76, 0.34);

    let buffer = renderer.next_buffer();
    buffer.clear(bg);

    let packed_options = 0b1_1111u32;
    buffer.draw_box(
        0,
        0,
        width as u32,
        height as u32,
        &BORDER_SIMPLE,
        packed_options,
        border,
        bg,
        Some(title),
    );

    let max_width = width.saturating_sub(4) as usize;
    let mut y = 2u32;

    let title_line = truncate_width(title, max_width);
    buffer.draw_text(&title_line, 3, y, text, None, ATTR_BOLD);
    y += 2;

    for line in lines {
        if y >= height.saturating_sub(3) as u32 {
            break;
        }
        let line = truncate_width(line, max_width);
        buffer.draw_text(&line, 3, y, text, None, 0);
        y += 1;
    }

    let hint = if default_yes {
        "Enter/Y = yes, N/Esc = no"
    } else {
        "Enter/N = no, Y = yes"
    };
    let hint_line = truncate_width(hint, max_width);
    let hint_y = height.saturating_sub(2) as u32;
    buffer.draw_text(&hint_line, 3, hint_y, muted, None, 0);

    let action = if default_yes {
        "[Y] Confirm"
    } else {
        "[N] Cancel"
    };
    let action_line = truncate_width(action, max_width);
    buffer.draw_text(
        &action_line,
        3,
        hint_y.saturating_sub(1),
        accent,
        None,
        ATTR_BOLD,
    );

    renderer.render(true);

    let answer = loop {
        match event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => break default_yes,
                KeyCode::Char('y') | KeyCode::Char('Y') => break true,
                KeyCode::Char('n') | KeyCode::Char('N') => break false,
                KeyCode::Esc => break false,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break false,
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break default_yes,
        }
    };

    renderer.clear_terminal();
    renderer.suspend();

    Some(answer)
}

struct RawModeGuard;

impl RawModeGuard {
    fn new() -> std::io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn truncate_width(input: &str, max: usize) -> String {
    if input.len() <= max {
        return input.to_string();
    }
    input.chars().take(max).collect::<String>()
}
