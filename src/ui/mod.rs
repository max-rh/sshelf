//! Rendering. `render` is a pure function of `&App` so it can be exercised with
//! ratatui's `TestBackend` (no real terminal needed).

mod browse;
pub(crate) mod forward_popup;
pub(crate) mod forwards;
mod help;
mod list;
pub(crate) mod settings;
pub(crate) mod sites;
mod transfer;
mod widgets;
pub(crate) mod wizard;

use std::sync::OnceLock;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{App, ConfirmDelete, Screen};

/// Split `text` into styled spans, applying `hl` to characters whose (char) index is in
/// `indices` and `base` to the rest. Shared by the host list and the file browser.
pub(crate) fn highlight(text: &str, indices: &[u32], base: Style, hl: Style) -> Vec<Span<'static>> {
    use std::collections::HashSet;
    let set: HashSet<u32> = indices.iter().copied().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut cur_hl = false;
    for (i, ch) in text.chars().enumerate() {
        let is_hl = set.contains(&(i as u32));
        if !cur.is_empty() && is_hl != cur_hl {
            spans.push(Span::styled(
                std::mem::take(&mut cur),
                if cur_hl { hl } else { base },
            ));
        }
        cur_hl = is_hl;
        cur.push(ch);
    }
    if !cur.is_empty() {
        spans.push(Span::styled(cur, if cur_hl { hl } else { base }));
    }
    spans
}

/// Accent color, fixed once at first render from `config.accent`.
static ACCENT: OnceLock<Color> = OnceLock::new();

/// The configured accent color (defaults to cyan until set).
pub(crate) fn accent() -> Color {
    *ACCENT.get().unwrap_or(&Color::Cyan)
}

fn parse_color(name: &str) -> Color {
    match name.trim().to_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        _ => Color::Cyan,
    }
}

pub fn render(frame: &mut Frame, app: &App) {
    let _ = ACCENT.set(parse_color(&app.config.accent));
    // The transfer screen owns the whole frame while open.
    if let Some(t) = &app.transfer {
        transfer::render(frame, t);
        return;
    }
    // Full-screen modals.
    if let Some(w) = &app.wizard {
        wizard::render(frame, w);
        return;
    }
    if let Some(s) = &app.settings {
        settings::render(frame, s);
        return;
    }
    if let Some(m) = &app.sites_manager {
        sites::render(frame, m);
        return;
    }
    if let Some(p) = &app.forward_popup {
        forward_popup::render(frame, p);
        return;
    }
    if let Some(m) = &app.forwards_manager {
        forwards::render(frame, m);
        return;
    }
    list::render(frame, app);
    if let Some(c) = &app.confirm {
        render_confirm(frame, c);
    } else if app.screen == Screen::Help {
        help::render(frame, app);
    }
}

fn render_confirm(frame: &mut Frame, c: &ConfirmDelete) {
    let area = centered(frame.area(), 52, 6);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" delete host ");
    let lines = vec![
        Line::from(format!("Delete \"{}\"?", c.name)),
        Line::from(""),
        Line::from("y = delete     any other key = cancel"),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::default().fg(Color::Red)),
        area,
    );
}

/// A centered rectangle of at most `width` x `height` within `area`.
pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}
