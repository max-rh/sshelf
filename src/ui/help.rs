//! The help overlay (toggled with F1).

use ratatui::Frame;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::centered;
use crate::app::App;

pub fn render(frame: &mut Frame, app: &App) {
    let area = centered(frame.area(), 62, 34);
    frame.render_widget(Clear, area);

    let accent = Style::default().fg(super::accent());
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = vec![
        Line::from(Span::styled(" sshelf — keybindings", accent)),
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
        Line::from("  F2              settings (hosts file, tmux mode)"),
        Line::from("  F3              manage sites"),
        Line::from("  F4              manage port forwards"),
        Line::from("  esc             clear query, then quit"),
        Line::from("  ^c              quit"),
        Line::from(""),
        Line::from(Span::styled(" transfer screen (^t)", accent)),
        Line::from("  tab             switch pane (local ↔ remote)"),
        Line::from("  space           mark / unmark the selected entry"),
        Line::from("  ^a              mark everything shown (again: clear)"),
        Line::from("  ^s              send the marked entries, else the one"),
        Line::from("  F7 / ^f         create a directory in this pane"),
        Line::from("  → / ↵           open a directory  ·  ←  go up"),
        Line::from("  esc             cancel, then marks, filter, close"),
        Line::from(""),
        Line::from(Span::styled(" tmux", accent)),
    ];
    // The one place the mode is visible without opening the settings screen.
    lines.push(Line::from(format!(
        "  mode            {} (F2 to change)",
        app.config.tmux.as_str()
    )));
    lines.push(Line::from(if app.in_tmux {
        "  ↵ opens a new tmux window/pane; sshelf stays up"
    } else {
        "  window/pane modes apply only inside tmux"
    }));
    lines.push(Line::from("  2FA + vault hosts always connect in place"));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  press any key to close", dim)));

    let block = Block::default().borders(Borders::ALL).title(" help ");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Tmux};
    use crate::paths::Paths;
    use crate::state::FrecencyState;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn snapshot(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(f, app)).unwrap();
        let buf = term.backend().buffer();
        let width = buf.area.width as usize;
        buf.content()
            .chunks(width)
            .map(|r| r.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app_with(tmux: Tmux, in_tmux: bool) -> App {
        let dir = std::env::temp_dir().join(format!("sshelf-help-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = App::new(
            Vec::new(),
            Vec::new(),
            FrecencyState::default(),
            Config {
                tmux,
                ..Config::default()
            },
            Paths {
                config_dir: dir.clone(),
                data_dir: dir,
                config_file_override: None,
            },
        );
        app.in_tmux = in_tmux;
        app
    }

    #[test]
    fn every_new_key_is_documented_in_the_overlay() {
        let snap = snapshot(&app_with(Tmux::Off, false), 70, 38);
        for key in ["space", "^a", "^s", "F7 / ^f", "tab"] {
            assert!(snap.contains(key), "help should list {key}:\n{snap}");
        }
        assert!(snap.contains("transfer screen"));
        assert!(snap.contains("mark"));
    }

    #[test]
    fn the_overlay_reports_the_live_tmux_mode() {
        let outside = snapshot(&app_with(Tmux::Window, false), 70, 38);
        assert!(outside.contains("window (F2 to change)"), "{outside}");
        assert!(outside.contains("only inside tmux"), "{outside}");

        let inside = snapshot(&app_with(Tmux::Pane, true), 70, 38);
        assert!(inside.contains("pane (F2 to change)"), "{inside}");
        assert!(inside.contains("sshelf stays up"), "{inside}");
        assert!(inside.contains("connect in place"), "{inside}");
    }
}
