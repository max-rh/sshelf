//! The 2FA verification-code popup, shown just before connecting to a host flagged
//! `requires_2fa`. The code is collected here — while the TUI is still alive — and handed to the
//! exec'd `ssh` through the askpass helper; sshelf never proxies the live session. See D-022.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::centered;
use super::widgets::TextField;

const LABEL: &str = "Verification code: ";

pub enum TwoFactorOutcome {
    Continue,
    Cancel,
    /// Connect now, supplying this one-time code to the verification prompt.
    Submit(String),
}

pub struct TwoFactorPopup {
    host_idx: usize,
    host_name: String,
    code: TextField,
    error: Option<String>,
}

impl TwoFactorPopup {
    pub fn new(host_idx: usize, host_name: String) -> Self {
        TwoFactorPopup {
            host_idx,
            host_name,
            code: TextField::new(),
            error: None,
        }
    }

    /// The host this code is for (the app resolves it back to a connect).
    pub fn host_idx(&self) -> usize {
        self.host_idx
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> TwoFactorOutcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return TwoFactorOutcome::Cancel;
        }
        match key.code {
            KeyCode::Esc => return TwoFactorOutcome::Cancel,
            KeyCode::Enter => {
                let code = self.code.value.trim().to_string();
                if code.is_empty() {
                    self.error = Some("enter the verification code".into());
                } else {
                    return TwoFactorOutcome::Submit(code);
                }
            }
            code => {
                self.code.handle(code);
            }
        }
        TwoFactorOutcome::Continue
    }
}

pub fn render(frame: &mut Frame, p: &TwoFactorPopup) {
    let width = frame.area().width.saturating_sub(6).clamp(46, 74);
    let area = centered(frame.area(), width, 7);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" 2FA · {} ", p.host_name));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let dim = Style::default().fg(Color::DarkGray);
    let accent = Style::default()
        .fg(super::accent())
        .add_modifier(Modifier::BOLD);

    let value = if p.code.value.is_empty() {
        Span::styled("the code your authenticator app shows", dim)
    } else {
        Span::raw(p.code.value.clone())
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(LABEL, accent), value])),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );

    if let Some(err) = &p.error {
        frame.render_widget(
            Paragraph::new(format!("⚠ {err}")).style(Style::default().fg(Color::Red)),
            Rect {
                x: inner.x,
                y: inner.y + 2,
                width: inner.width,
                height: 1,
            },
        );
    }

    frame.render_widget(
        Paragraph::new("↵ connect · esc cancel").style(dim),
        Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        },
    );

    let col = (inner.x + LABEL.chars().count() as u16 + p.code.cursor as u16)
        .min(inner.x + inner.width.saturating_sub(1));
    frame.set_cursor_position((col, inner.y));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn type_str(p: &mut TwoFactorPopup, s: &str) {
        for c in s.chars() {
            p.handle_key(k(KeyCode::Char(c)));
        }
    }

    #[test]
    fn submit_returns_the_code() {
        let mut p = TwoFactorPopup::new(2, "vpn".into());
        type_str(&mut p, "654321");
        match p.handle_key(k(KeyCode::Enter)) {
            TwoFactorOutcome::Submit(code) => assert_eq!(code, "654321"),
            _ => panic!("expected Submit"),
        }
        assert_eq!(p.host_idx(), 2);
    }

    #[test]
    fn empty_enter_keeps_open_with_error() {
        let mut p = TwoFactorPopup::new(0, "vpn".into());
        assert!(matches!(
            p.handle_key(k(KeyCode::Enter)),
            TwoFactorOutcome::Continue
        ));
        assert!(p.error.is_some());
    }

    #[test]
    fn esc_cancels() {
        let mut p = TwoFactorPopup::new(0, "vpn".into());
        assert!(matches!(
            p.handle_key(k(KeyCode::Esc)),
            TwoFactorOutcome::Cancel
        ));
    }

    #[test]
    fn renders_snapshot() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut p = TwoFactorPopup::new(0, "prod-vpn".into());
        type_str(&mut p, "123456");
        let mut term = Terminal::new(TestBackend::new(60, 9)).unwrap();
        term.draw(|f| render(f, &p)).unwrap();
        let buf = term.backend().buffer();
        let width = buf.area.width as usize;
        let snapshot: String = buf
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(snapshot.contains("2FA · prod-vpn"));
        assert!(snapshot.contains("Verification code:"));
        assert!(snapshot.contains("123456"));
    }
}
