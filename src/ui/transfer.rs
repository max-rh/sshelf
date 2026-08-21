//! Rendering for the dual-pane transfer screen: two directory panes side by side, a progress/
//! status line, and a hint bar. `render` pulls the pieces it needs off the screen into a
//! borrowed [`View`] so the layout can be exercised with `TestBackend` (no live worker).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph};

use super::widgets::TextField;
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
    /// The inline "new directory" input, shown at the bottom of the focused pane.
    mkdir: Option<&'a TextField>,
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
            mkdir: screen.mkdir_input(),
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
    let local_focused = view.focus == Side::Local;
    render_pane(
        frame,
        cols[0],
        view.local,
        "local",
        local_focused,
        false,
        local_focused.then_some(view.mkdir).flatten(),
    );
    render_pane(
        frame,
        cols[1],
        view.remote,
        view.target,
        !local_focused,
        view.connecting,
        (!local_focused).then_some(view.mkdir).flatten(),
    );

    render_footer(frame, rows[1], view);

    frame.render_widget(
        Paragraph::new(
            "tab switch · space mark · ^a all · ^s send · ^f mkdir · → open · ← up · esc back",
        )
        .style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
}

#[allow(clippy::fn_params_excessive_bools)]
fn render_pane(
    frame: &mut Frame,
    area: Rect,
    pane: &Pane,
    title: &str,
    focused: bool,
    connecting: bool,
    mkdir: Option<&TextField>,
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
    let marked = pane.marked_count();
    let heading = if marked > 0 {
        format!(" {title} · {marked} marked ")
    } else {
        format!(" {title} ")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(heading, title_style));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1),                          // cwd
        Constraint::Length(1),                          // filter
        Constraint::Min(0),                             // listing
        Constraint::Length(u16::from(mkdir.is_some())), // new-directory input
    ])
    .split(inner);

    if let Some(field) = mkdir {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("new dir: ", Style::default().fg(accent())),
                Span::raw(field.value.clone()),
            ])),
            rows[3],
        );
        frame.set_cursor_position((
            (rows[3].x + 9 + field.cursor as u16).min(rows[3].right().saturating_sub(1)),
            rows[3].y,
        ));
    }

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
        .map(|(e, label, is_marked)| {
            // Marked rows carry both a gutter glyph and the accent color, so they stand out
            // whether or not the terminal renders the bullet well.
            let base = if *is_marked {
                Style::default().fg(accent()).add_modifier(Modifier::BOLD)
            } else if e.is_symlink {
                Style::default().fg(Color::DarkGray)
            } else if e.is_dir {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let idx = search::match_indices(label, pane.query(), &mut matcher);
            let mut spans = vec![Span::styled(
                if *is_marked { "•" } else { " " },
                Style::default().fg(accent()).add_modifier(Modifier::BOLD),
            )];
            spans.extend(highlight(label, &idx, base, hl));
            ListItem::new(Line::from(spans))
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
            mkdir: None,
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
            mkdir: None,
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
            mkdir: None,
        };
        assert!(snapshot(&view, 20, 5).contains("terminal too small"));
    }

    #[test]
    fn marked_rows_carry_a_gutter_glyph_and_a_count() {
        let mut local = pane("/home/me", &[("docs", true), ("readme.md", false)]);
        local.move_sel(1); // past `..`, onto docs/
        local.toggle_mark();
        let remote = pane("/srv", &[]);
        let view = View {
            local: &local,
            remote: &remote,
            focus: Side::Local,
            target: "deploy@host",
            connecting: false,
            status: None,
            active: None,
            mkdir: None,
        };
        let snap = snapshot(&view, 80, 20);
        assert!(
            snap.contains("•docs/"),
            "the marked row is flagged:\n{snap}"
        );
        assert!(snap.contains("1 marked"), "the pane title counts them");
        assert!(snap.contains("space mark"), "the hint bar teaches the key");
    }

    #[test]
    fn the_new_directory_input_sits_under_the_focused_pane() {
        let local = pane("/home/me", &[("docs", true)]);
        let remote = pane("/srv", &[]);
        let field = TextField::with("releases");
        let view = View {
            local: &local,
            remote: &remote,
            focus: Side::Local,
            target: "deploy@host",
            connecting: false,
            status: None,
            active: None,
            mkdir: Some(&field),
        };
        let snap = snapshot(&view, 80, 20);
        assert!(snap.contains("new dir: releases"), "{snap}");
        // Only the focused pane shows it — one input, one target directory.
        assert_eq!(snap.matches("new dir:").count(), 1);
    }
}
