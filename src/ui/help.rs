//! The help overlay (toggled with F1).

use ratatui::Frame;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::centered;
use crate::app::App;

pub fn render(frame: &mut Frame, _app: &App) {
    let area = centered(frame.area(), 58, 23);
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::from(Span::styled(
            " sshelf — keybindings",
            Style::default().fg(super::accent()),
        )),
        Line::from(""),
        Line::from("  type            filter the list (fuzzy)"),
        Line::from("  tag:NAME        filter by tag (combine with text)"),
        Line::from("  site:NAME       filter by site"),
        Line::from("  ↑ / ↓  ^p / ^n  move selection"),
        Line::from("  ↵               connect"),
        Line::from("  ^a / ^e / ^d    add / edit / delete host"),
        Line::from("  ^y              yank the ssh command"),
        Line::from("  ^t              transfer files (SFTP)"),
        Line::from("  ^f              port forward (runs in the background)"),
        Line::from("  ^o              import from ~/.ssh/config"),
        Line::from("  F1              this help"),
        Line::from("  F2              settings (config & hosts file)"),
        Line::from("  F3              manage sites"),
        Line::from("  F4              manage port forwards"),
        Line::from("  esc             clear query, then quit"),
        Line::from("  ^c              quit"),
        Line::from(""),
        Line::from(Span::styled(
            "  press any key to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default().borders(Borders::ALL).title(" help ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
