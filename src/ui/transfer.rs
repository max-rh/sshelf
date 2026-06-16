//! Rendering for the dual-pane transfer screen: two directory panes side by side, a progress/
//! status line, and a hint bar. `render` pulls the pieces it needs off the screen into a
//! borrowed [`View`] so the layout can be exercised with `TestBackend` (no live worker).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph};

use super::{accent, centered, highlight};
use crate::search;
use crate::transfer::{Pane, Progress, Side, TransferScreen};

/// Everything the renderer needs, borrowed from the screen (and easy to build in tests).
struct View<'a> {
    local: &'a Pane,
    remote: &'a Pane,
    focus: Side,
    target: &'a str,
    connecting: bool,
    status: Option<&'a str>,
    active: Option<(Progress, &'a str)>,
}

pub fn render(frame: &mut Frame, screen: &TransferScreen) {
    draw(
        frame,
        &View {
            local: screen.local_pane(),
            remote: screen.remote_pane(),
            focus: screen.focused_side(),
            target: screen.target(),
            connecting: screen.is_connecting(),
            status: screen.status(),
            active: screen.active(),
        },
    );
}

fn draw(frame: &mut Frame, view: &View) {
    let area = frame.area();
    if area.width < 50 || area.height < 10 {
        let msg = Paragraph::new("terminal too small").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(msg, centered(area, 18, 1));
        return;
    }

    let rows = Layout::vertical([
        Constraint::Min(0),    // the two panes
        Constraint::Length(2), // progress / status
        Constraint::Length(1), // hint bar
    ])
    .split(area);

    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[0]);
    render_pane(
        frame,
        cols[0],
        view.local,
        "local",
        view.focus == Side::Local,
        false,
    );
    render_pane(
        frame,
        cols[1],
        view.remote,
        view.target,
        view.focus == Side::Remote,
        view.connecting,
    );

    render_footer(frame, rows[1], view);

    frame.render_widget(
        Paragraph::new("tab switch · ↑↓ move · → open · ^s send · ← up · esc cancel/close")
            .style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
}

fn render_pane(
    frame: &mut Frame,
    area: Rect,
    pane: &Pane,
    title: &str,
    focused: bool,
    connecting: bool,
) {
    let border = if focused {
        Style::default().fg(accent())
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title_style = if focused {
        Style::default().fg(accent()).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(format!(" {title} "), title_style));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1), // cwd
        Constraint::Length(1), // filter
        Constraint::Min(0),    // listing
    ])
    .split(inner);

    frame.render_widget(
        Paragraph::new(truncate_left(
            &pane.cwd.to_string_lossy(),
            rows[0].width as usize,
        ))
        .style(Style::default().fg(accent())),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(accent())),
            Span::raw(pane.query().to_string()),
        ])),
        rows[1],
    );

    if connecting {
        return message(frame, rows[2], "connecting…", Color::DarkGray);
    }
    if let Some(err) = &pane.error {
        return message(frame, rows[2], err, Color::Red);
    }
    if pane.loading {
        return message(frame, rows[2], "loading…", Color::DarkGray);
    }

    let listing = pane.rows();
    let mut matcher = search::matcher();
    let hl = Style::default().fg(accent()).add_modifier(Modifier::BOLD);
    let items: Vec<ListItem> = listing
        .iter()
        .map(|(e, label)| {
            let base = if e.is_symlink {
                Style::default().fg(Color::DarkGray)
            } else if e.is_dir {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let idx = search::match_indices(label, pane.query(), &mut matcher);
            ListItem::new(Line::from(highlight(label, &idx, base, hl)))
        })
        .collect();
    let list = List::new(items)
        .highlight_symbol("▸ ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    // Only the focused pane shows a selection.
    if focused && !listing.is_empty() {
        state.select(Some(pane.selected()));
    }
    frame.render_stateful_widget(list, rows[2], &mut state);
}

fn render_footer(frame: &mut Frame, area: Rect, view: &View) {
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    if let Some((progress, label)) = view.active {
        let info = if progress.bytes_total > 0 {
            format!(
                "{label}   {} / {}",
                human(progress.bytes_done),
                human(progress.bytes_total)
            )
        } else {
            format!("{label}   {} transferred…", human(progress.bytes_done))
        };
        frame.render_widget(
            Paragraph::new(info).style(Style::default().fg(accent())),
            rows[0],
        );
        if progress.bytes_total > 0 {
            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(accent()))
                .ratio(progress.percent() as f64 / 100.0)
                .label(format!("{}%", progress.percent()));
            frame.render_widget(gauge, rows[1]);
        } else {
            frame.render_widget(
                Paragraph::new("esc to cancel").style(Style::default().fg(Color::DarkGray)),
                rows[1],
            );
        }
    } else if let Some(status) = view.status {
        frame.render_widget(
            Paragraph::new(status).style(Style::default().fg(accent())),
            rows[0],
        );
    }
}

fn message(frame: &mut Frame, area: Rect, text: &str, color: Color) {
    frame.render_widget(Paragraph::new(text).style(Style::default().fg(color)), area);
}

/// Truncate from the left so a long path's tail (the part that matters) stays visible.
fn truncate_left(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let tail: String = s.chars().rev().take(width.saturating_sub(1)).collect();
    format!("…{}", tail.chars().rev().collect::<String>())
}

/// Bytes as a short human-readable size.
fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::PaneEntry;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn pane(cwd: &str, names: &[(&str, bool)]) -> Pane {
        let mut p = Pane::new(PathBuf::from(cwd));
        p.set_entries(
            names
                .iter()
                .map(|&(name, is_dir)| PaneEntry {
                    name: name.into(),
                    is_dir,
                    is_symlink: false,
                    size: 10,
                })
                .collect(),
        );
        p
    }

    fn snapshot(view: &View, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, view)).unwrap();
        let buf = term.backend().buffer();
        let width = buf.area.width as usize;
        buf.content()
            .chunks(width)
            .map(|r| r.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_both_panes_and_hint() {
        let local = pane("/home/me", &[("docs", true), ("readme.md", false)]);
        let remote = pane("/srv", &[("logs", true), ("app.conf", false)]);
        let view = View {
            local: &local,
            remote: &remote,
            focus: Side::Local,
            target: "deploy@host",
            connecting: false,
            status: None,
            active: None,
        };
        let snap = snapshot(&view, 80, 20);
        assert!(snap.contains("local"));
        assert!(snap.contains("deploy@host"));
        assert!(snap.contains("docs/"));
        assert!(snap.contains("app.conf"));
        assert!(snap.contains("send"));
    }

    #[test]
    fn shows_progress_while_transferring() {
        let local = pane("/home/me", &[("big.iso", false)]);
        let remote = pane("/srv", &[]);
        let view = View {
            local: &local,
            remote: &remote,
            focus: Side::Local,
            target: "deploy@host",
            connecting: false,
            status: None,
            active: Some((
                Progress {
                    bytes_done: 512,
                    bytes_total: 1024,
                },
                "big.iso → deploy@host",
            )),
        };
        let snap = snapshot(&view, 80, 20);
        assert!(snap.contains("big.iso → deploy@host"));
        assert!(snap.contains("50%"));
    }

    #[test]
    fn tiny_terminal_clamps() {
        let local = pane("/", &[]);
        let remote = pane("/", &[]);
        let view = View {
            local: &local,
            remote: &remote,
            focus: Side::Local,
            target: "h",
            connecting: true,
            status: None,
            active: None,
        };
        assert!(snapshot(&view, 20, 5).contains("terminal too small"));
    }
}
