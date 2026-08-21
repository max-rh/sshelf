//! The settings screen (F2): configure sshelf itself — the hosts-file location and where tmux
//! mode opens connections. The config-file path is shown read-only because it's chosen *before*
//! the config is read (via `--config` / `$SSHELF_CONFIG`).

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::widgets::TextField;
use super::{accent, centered};
use crate::config::Tmux;

const VALUE_COL: u16 = 15;
const LABEL_W: usize = 12;
/// Screen rows (relative to the box's inner area) each editable field sits on.
const HOSTS_ROW: u16 = 3;
const TMUX_ROW: u16 = 5;

pub enum SettingsOutcome {
    Continue,
    Cancel,
    /// Save preferences; `hosts_file` is `None` to use the default location.
    Save {
        hosts_file: Option<String>,
        tmux: Tmux,
    },
}

/// Which field the cursor is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    HostsFile,
    Tmux,
}

pub struct Settings {
    /// Active config-file path (display only).
    config_path: String,
    /// Default hosts path, shown as a placeholder when the field is blank.
    default_hosts: String,
    hosts_file: TextField,
    tmux: Tmux,
    focus: Field,
}

impl Settings {
    pub fn new(
        config_path: String,
        hosts_file: Option<String>,
        default_hosts: String,
        tmux: Tmux,
    ) -> Self {
        Settings {
            config_path,
            default_hosts,
            hosts_file: TextField::with(hosts_file.unwrap_or_default()),
            tmux,
            focus: Field::HostsFile,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('s')) {
            return self.save();
        }
        match key.code {
            KeyCode::Esc => return SettingsOutcome::Cancel,
            KeyCode::Enter => return self.save(),
            KeyCode::Tab | KeyCode::Down => self.focus = self.other_field(),
            KeyCode::BackTab | KeyCode::Up => self.focus = self.other_field(),
            code => match self.focus {
                Field::HostsFile => {
                    self.hosts_file.handle(code);
                }
                // The tmux field is a cycling toggle: it has three states, not free text.
                Field::Tmux => {
                    if matches!(code, KeyCode::Char(' ') | KeyCode::Right | KeyCode::Left) {
                        self.tmux = self.tmux.next();
                    }
                }
            },
        }
        SettingsOutcome::Continue
    }

    fn other_field(&self) -> Field {
        match self.focus {
            Field::HostsFile => Field::Tmux,
            Field::Tmux => Field::HostsFile,
        }
    }

    fn save(&self) -> SettingsOutcome {
        let v = self.hosts_file.value.trim();
        SettingsOutcome::Save {
            hosts_file: (!v.is_empty()).then(|| v.to_string()),
            tmux: self.tmux,
        }
    }
}

/// Truncate `s` from the left to fit `width`, prefixing `…` when shortened.
fn fit_left(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let tail: String = s.chars().rev().take(width.saturating_sub(1)).collect();
    format!("…{}", tail.chars().rev().collect::<String>())
}

pub fn render(frame: &mut Frame, s: &Settings) {
    let width = frame.area().width.saturating_sub(6).clamp(56, 100);
    let area = centered(frame.area(), width, 13);
    frame.render_widget(Clear, area);
    let block = Block::default().borders(Borders::ALL).title(" settings ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let acc = Style::default().fg(accent()).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let row = |i: u16| Rect {
        x: inner.x,
        y: inner.y + i,
        width: inner.width,
        height: 1,
    };
    let val_w = inner.width.saturating_sub(VALUE_COL) as usize;

    // Config file (read-only info).
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::raw(format!("{:<LABEL_W$} ", "Config file")),
            Span::styled(fit_left(&s.config_path, val_w), dim),
        ])),
        row(0),
    );
    frame.render_widget(
        Paragraph::new("    (read-only — set via --config or $SSHELF_CONFIG)").style(dim),
        row(1),
    );

    // Hosts file (editable text).
    let (hosts_val, hosts_style) = if s.hosts_file.value.is_empty() {
        (
            format!(
                "default · {}",
                fit_left(&s.default_hosts, val_w.saturating_sub(10))
            ),
            dim,
        )
    } else {
        (s.hosts_file.value.clone(), Style::default())
    };
    frame.render_widget(
        Paragraph::new(field_line(
            s.focus == Field::HostsFile,
            "Hosts file",
            hosts_val,
            hosts_style,
            acc,
        )),
        row(HOSTS_ROW),
    );

    // tmux mode (a three-state toggle).
    frame.render_widget(
        Paragraph::new(field_line(
            s.focus == Field::Tmux,
            "tmux",
            s.tmux.as_str().to_string(),
            Style::default(),
            acc,
        )),
        row(TMUX_ROW),
    );
    frame.render_widget(
        Paragraph::new(match s.tmux {
            Tmux::Off => "    (space cycles — off / window / pane: where Enter opens a connection)",
            Tmux::Window => "    (space cycles — inside tmux, Enter opens a new window)",
            Tmux::Pane => "    (space cycles — inside tmux, Enter splits off a new pane)",
        })
        .style(dim),
        row(TMUX_ROW + 1),
    );

    frame.render_widget(
        Paragraph::new("tab next field · ↵ or ^s save · esc cancel").style(dim),
        row(inner.height.saturating_sub(1)),
    );

    // The cursor belongs on the text field only; the toggle has no insertion point.
    if s.focus == Field::HostsFile {
        let cx = inner.x + VALUE_COL + s.hosts_file.cursor as u16;
        frame.set_cursor_position((
            cx.min(inner.x + inner.width.saturating_sub(1)),
            inner.y + HOSTS_ROW,
        ));
    }
}

/// One labelled form row: an accent marker + label when focused, dim otherwise.
fn field_line(
    focused: bool,
    label: &str,
    value: String,
    value_style: Style,
    acc: Style,
) -> Line<'static> {
    let label_style = if focused { acc } else { Style::default() };
    Line::from(vec![
        Span::styled(if focused { "▸ " } else { "  " }, acc),
        Span::styled(format!("{label:<LABEL_W$} "), label_style),
        Span::styled(value, value_style),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn new_settings() -> Settings {
        Settings::new(
            "/home/u/.config/sshelf/config.toml".into(),
            None,
            "/home/u/.config/sshelf/hosts.toml".into(),
            Tmux::Off,
        )
    }

    #[test]
    fn empty_saves_none() {
        let mut s = new_settings();
        match s.handle_key(k(KeyCode::Enter)) {
            SettingsOutcome::Save { hosts_file, tmux } => {
                assert!(hosts_file.is_none());
                assert_eq!(tmux, Tmux::Off);
            }
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn typed_path_saves_some() {
        let mut s = new_settings();
        for c in "/data/hosts.toml".chars() {
            s.handle_key(k(KeyCode::Char(c)));
        }
        match s.handle_key(ctrl(KeyCode::Char('s'))) {
            SettingsOutcome::Save { hosts_file, .. } => {
                assert_eq!(hosts_file.as_deref(), Some("/data/hosts.toml"));
            }
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn tab_reaches_the_tmux_field_and_space_cycles_it() {
        let mut s = new_settings();
        s.handle_key(k(KeyCode::Tab));
        s.handle_key(k(KeyCode::Char(' ')));
        match s.handle_key(ctrl(KeyCode::Char('s'))) {
            SettingsOutcome::Save { tmux, .. } => assert_eq!(tmux, Tmux::Window),
            _ => panic!("expected save"),
        }
        // …and it wraps back around to off.
        let mut s = new_settings();
        s.handle_key(k(KeyCode::Tab));
        for _ in 0..3 {
            s.handle_key(k(KeyCode::Char(' ')));
        }
        match s.handle_key(k(KeyCode::Enter)) {
            SettingsOutcome::Save { tmux, .. } => assert_eq!(tmux, Tmux::Off),
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn typing_on_the_tmux_field_does_not_edit_the_path() {
        let mut s = new_settings();
        s.handle_key(k(KeyCode::Tab));
        for c in "/oops".chars() {
            s.handle_key(k(KeyCode::Char(c)));
        }
        match s.handle_key(k(KeyCode::Enter)) {
            SettingsOutcome::Save { hosts_file, .. } => assert!(hosts_file.is_none()),
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn esc_cancels() {
        let mut s = new_settings();
        assert!(matches!(
            s.handle_key(k(KeyCode::Esc)),
            SettingsOutcome::Cancel
        ));
    }

    #[test]
    fn renders_and_writes_snapshot() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let s = new_settings();
        let mut term = Terminal::new(TestBackend::new(78, 16)).unwrap();
        term.draw(|f| render(f, &s)).unwrap();
        let buf = term.backend().buffer();
        let width = buf.area.width as usize;
        let snapshot: String = buf
            .content()
            .chunks(width)
            .map(|r| r.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(snapshot.contains("settings"));
        assert!(snapshot.contains("Hosts file"));
        assert!(snapshot.contains("Config file"));
        assert!(snapshot.contains("tmux"));
        if let Ok(dir) = std::env::var("CARGO_MANIFEST_DIR") {
            let p = std::path::Path::new(&dir).join("target/settings-snapshot.txt");
            let _ = std::fs::write(p, &snapshot);
        }
    }
}
