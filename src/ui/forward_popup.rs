//! The "new port forward" popup (a self-contained component: state + key handling + render).
//!
//! A small form opened with `Ctrl-f` on the selected host. The user picks a kind (Local /
//! Remote / Dynamic — cycled with ←/→) and fills in the ports/host; the active fields depend on
//! the kind. `Ctrl-s` (or Enter on the last field) confirms, returning the [`ForwardSpec`] for the
//! app to spawn; a validation or bind/auth error keeps the popup open with the message.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::centered;
use super::widgets::TextField;
use crate::forwards::{ForwardKind, ForwardSpec};

/// Columns before a field's value: marker(2) + padded label(12) + space(1).
const VALUE_COL: u16 = 15;
const LABEL_W: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Kind,
    Bind,
    ListenPort,
    TargetHost,
    TargetPort,
}

impl Field {
    fn label(self, kind: ForwardKind) -> &'static str {
        match self {
            Field::Kind => "Type",
            Field::Bind => "Bind addr",
            Field::ListenPort => match kind {
                ForwardKind::Remote => "Remote port",
                _ => "Local port",
            },
            Field::TargetHost => match kind {
                ForwardKind::Remote => "Local host",
                _ => "Remote host",
            },
            Field::TargetPort => match kind {
                ForwardKind::Remote => "Local port",
                _ => "Remote port",
            },
        }
    }

    fn placeholder(self, kind: ForwardKind) -> &'static str {
        match self {
            Field::Kind => "←/→ Local · Remote · Dynamic",
            Field::Bind => "optional · listen interface (default 127.0.0.1)",
            Field::ListenPort => match kind {
                ForwardKind::Remote => "required · port to open on the server (e.g. 9090)",
                ForwardKind::Dynamic => "required · local SOCKS port (e.g. 1080)",
                ForwardKind::Local => "required · local port to open (e.g. 8080)",
            },
            Field::TargetHost => match kind {
                ForwardKind::Remote => "optional · host reached from here (default localhost)",
                _ => "optional · host reached from the server (default localhost)",
            },
            Field::TargetPort => "required · destination port (e.g. 3306)",
        }
    }
}

/// Fields shown for a kind, in display order.
fn active_fields(kind: ForwardKind) -> Vec<Field> {
    let mut v = vec![Field::Kind, Field::Bind, Field::ListenPort];
    if matches!(kind, ForwardKind::Local | ForwardKind::Remote) {
        v.push(Field::TargetHost);
        v.push(Field::TargetPort);
    }
    v
}

pub enum ForwardPopupOutcome {
    Continue,
    Cancel,
    /// Spawn the forward; the app resolves the host (it holds `host_idx`) and runs it.
    Create {
        kind: ForwardKind,
        spec: ForwardSpec,
    },
}

pub struct ForwardPopup {
    host_idx: usize,
    host_name: String,
    kind: ForwardKind,
    focus: usize,
    bind: TextField,
    listen_port: TextField,
    target_host: TextField,
    target_port: TextField,
    error: Option<String>,
}

impl ForwardPopup {
    pub fn new(host_idx: usize, host_name: String) -> Self {
        ForwardPopup {
            host_idx,
            host_name,
            kind: ForwardKind::Local,
            focus: 0,
            bind: TextField::new(),
            listen_port: TextField::new(),
            target_host: TextField::new(),
            target_port: TextField::new(),
            error: None,
        }
    }

    /// The host this forward is for (the app resolves site defaults + secrets from it).
    pub fn host_idx(&self) -> usize {
        self.host_idx
    }

    /// Re-show the popup with a bind/auth error returned by the spawn attempt.
    pub fn set_error(&mut self, msg: String) {
        self.error = Some(msg);
    }

    fn text_field_mut(&mut self, f: Field) -> Option<&mut TextField> {
        match f {
            Field::Bind => Some(&mut self.bind),
            Field::ListenPort => Some(&mut self.listen_port),
            Field::TargetHost => Some(&mut self.target_host),
            Field::TargetPort => Some(&mut self.target_port),
            Field::Kind => None,
        }
    }

    fn text_field(&self, f: Field) -> Option<&TextField> {
        match f {
            Field::Bind => Some(&self.bind),
            Field::ListenPort => Some(&self.listen_port),
            Field::TargetHost => Some(&self.target_host),
            Field::TargetPort => Some(&self.target_port),
            Field::Kind => None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ForwardPopupOutcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('s')) {
            return self.try_create();
        }
        let active = active_fields(self.kind);
        let last = active.len().saturating_sub(1);
        let focused = active[self.focus.min(last)];
        match key.code {
            KeyCode::Esc => return ForwardPopupOutcome::Cancel,
            KeyCode::Enter => {
                if self.focus >= last {
                    return self.try_create();
                }
                self.focus += 1;
            }
            KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % active.len(),
            KeyCode::BackTab | KeyCode::Up => {
                self.focus = (self.focus + active.len() - 1) % active.len()
            }
            code => match focused {
                Field::Kind => match code {
                    KeyCode::Left => self.kind = prev_kind(self.kind),
                    KeyCode::Right | KeyCode::Char(' ') => self.kind = next_kind(self.kind),
                    _ => {}
                },
                f => {
                    if let Some(tf) = self.text_field_mut(f) {
                        tf.handle(code);
                    }
                }
            },
        }
        // Switching kind may shrink the active set (Dynamic); keep focus in range.
        let len = active_fields(self.kind).len();
        if self.focus >= len {
            self.focus = len - 1;
        }
        ForwardPopupOutcome::Continue
    }

    fn try_create(&mut self) -> ForwardPopupOutcome {
        let listen_port = match parse_port(&self.listen_port.value) {
            Ok(p) => p,
            Err(e) => return self.fail(e, Field::ListenPort),
        };
        let bind = nonempty(&self.bind.value);
        let (target_host, target_port) =
            if matches!(self.kind, ForwardKind::Local | ForwardKind::Remote) {
                let tp = match parse_port(&self.target_port.value) {
                    Ok(p) => p,
                    Err(e) => return self.fail(e, Field::TargetPort),
                };
                (nonempty(&self.target_host.value), Some(tp))
            } else {
                (None, None)
            };
        let spec = ForwardSpec {
            bind,
            listen_port,
            target_host,
            target_port,
        };
        ForwardPopupOutcome::Create {
            kind: self.kind,
            spec,
        }
    }

    fn fail(&mut self, msg: &str, field: Field) -> ForwardPopupOutcome {
        self.error = Some(msg.to_string());
        if let Some(pos) = active_fields(self.kind).iter().position(|f| *f == field) {
            self.focus = pos;
        }
        ForwardPopupOutcome::Continue
    }
}

fn next_kind(k: ForwardKind) -> ForwardKind {
    let all = ForwardKind::ALL;
    let i = all.iter().position(|&x| x == k).unwrap_or(0);
    all[(i + 1) % all.len()]
}
fn prev_kind(k: ForwardKind) -> ForwardKind {
    let all = ForwardKind::ALL;
    let i = all.iter().position(|&x| x == k).unwrap_or(0);
    all[(i + all.len() - 1) % all.len()]
}

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn parse_port(s: &str) -> Result<u16, &'static str> {
    let t = s.trim();
    if t.is_empty() {
        return Err("port is required");
    }
    match t.parse::<u16>() {
        Ok(0) | Err(_) => Err("port must be a number between 1 and 65535"),
        Ok(p) => Ok(p),
    }
}

pub fn render(frame: &mut Frame, p: &ForwardPopup) {
    let active = active_fields(p.kind);
    let width = frame.area().width.saturating_sub(6).clamp(52, 86);
    let area = centered(frame.area(), width, active.len() as u16 + 7);
    frame.render_widget(Clear, area);

    let title = format!(" port forward · {} ", p.host_name);
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let accent = Style::default()
        .fg(super::accent())
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    for (row, &field) in active.iter().enumerate() {
        let focused = row == p.focus;
        let marker = if focused { "▸ " } else { "  " };
        let label_style = if focused { accent } else { Style::default() };
        let (value, is_placeholder) = field_value(p, field);
        let value_style = if is_placeholder {
            dim
        } else {
            Style::default()
        };
        let line = Line::from(vec![
            Span::styled(marker, accent),
            Span::styled(format!("{:<LABEL_W$} ", field.label(p.kind)), label_style),
            Span::styled(value, value_style),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect {
                x: inner.x,
                y: inner.y + row as u16,
                width: inner.width,
                height: 1,
            },
        );
    }

    let y = inner.y + active.len() as u16 + 1;
    if let Some(err) = &p.error {
        frame.render_widget(
            Paragraph::new(format!("⚠ {err}")).style(Style::default().fg(Color::Red)),
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            },
        );
    }
    frame.render_widget(
        Paragraph::new("Tab/↑↓ move · ←/→ change · ↵ next (runs on last) · ^s start · esc cancel")
            .style(dim),
        Rect {
            x: inner.x,
            y: y + 1,
            width: inner.width,
            height: 1,
        },
    );

    // Terminal cursor on the focused text field (not the kind chooser).
    if let Some(&field) = active.get(p.focus)
        && let Some(tf) = p.text_field(field)
    {
        let cx =
            (inner.x + VALUE_COL + tf.cursor as u16).min(inner.x + inner.width.saturating_sub(1));
        frame.set_cursor_position((cx, inner.y + p.focus as u16));
    }
}

/// (display string, is_placeholder) for a field.
fn field_value(p: &ForwardPopup, field: Field) -> (String, bool) {
    match field {
        Field::Kind => (
            format!("< {} >    (Local · Remote · Dynamic)", p.kind.label()),
            false,
        ),
        _ => {
            let tf = p.text_field(field).expect("text field");
            if tf.value.is_empty() {
                (field.placeholder(p.kind).to_string(), true)
            } else {
                (tf.value.clone(), false)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn local_create_builds_spec() {
        let mut p = ForwardPopup::new(0, "web".into());
        p.listen_port = TextField::with("8080");
        p.target_host = TextField::with("db");
        p.target_port = TextField::with("3306");
        match p.try_create() {
            ForwardPopupOutcome::Create { kind, spec } => {
                assert_eq!(kind, ForwardKind::Local);
                assert_eq!(spec.listen_port, 8080);
                assert_eq!(spec.target_host.as_deref(), Some("db"));
                assert_eq!(spec.target_port, Some(3306));
                assert_eq!(spec.bind, None);
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn dynamic_needs_only_listen_port() {
        let mut p = ForwardPopup::new(0, "web".into());
        p.kind = ForwardKind::Dynamic;
        p.listen_port = TextField::with("1080");
        match p.try_create() {
            ForwardPopupOutcome::Create { kind, spec } => {
                assert_eq!(kind, ForwardKind::Dynamic);
                assert_eq!(spec.listen_port, 1080);
                assert_eq!(spec.target_port, None);
            }
            _ => panic!("expected Create"),
        }
    }

    #[test]
    fn missing_listen_port_keeps_open_with_error() {
        let mut p = ForwardPopup::new(0, "web".into());
        assert!(matches!(p.try_create(), ForwardPopupOutcome::Continue));
        assert!(p.error.is_some());
    }

    #[test]
    fn kind_chooser_cycles_and_changes_active_set() {
        let mut p = ForwardPopup::new(0, "web".into());
        assert_eq!(p.kind, ForwardKind::Local);
        assert!(active_fields(p.kind).contains(&Field::TargetHost));
        p.handle_key(k(KeyCode::Right)); // Local -> Remote
        assert_eq!(p.kind, ForwardKind::Remote);
        p.handle_key(k(KeyCode::Right)); // Remote -> Dynamic
        assert_eq!(p.kind, ForwardKind::Dynamic);
        assert!(!active_fields(p.kind).contains(&Field::TargetHost));
    }

    #[test]
    fn esc_cancels() {
        let mut p = ForwardPopup::new(0, "web".into());
        assert!(matches!(
            p.handle_key(k(KeyCode::Esc)),
            ForwardPopupOutcome::Cancel
        ));
    }

    #[test]
    fn renders_snapshot() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut p = ForwardPopup::new(0, "prod-db".into());
        p.listen_port = TextField::with("8080");
        p.target_host = TextField::with("db");
        p.target_port = TextField::with("3306");

        let mut term = Terminal::new(TestBackend::new(72, 12)).unwrap();
        term.draw(|f| render(f, &p)).unwrap();
        let buf = term.backend().buffer();
        let width = buf.area.width as usize;
        let snapshot: String = buf
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(snapshot.contains("port forward"));
        assert!(snapshot.contains("prod-db"));
        assert!(snapshot.contains("Local port"));

        if let Ok(dir) = std::env::var("CARGO_MANIFEST_DIR") {
            let path = std::path::Path::new(&dir).join("target/forward-popup-snapshot.txt");
            let _ = std::fs::write(path, &snapshot);
        }
    }
}
