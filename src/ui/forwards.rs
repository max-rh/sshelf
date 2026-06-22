//! The port-forwards manager (F4): list every active background forward across hosts and stop
//! any of them. It renders from a snapshot the app refreshes each tick (so a forward that ends —
//! here, externally, or on its own — drops out live); stopping one returns its id for the app to
//! kill + remove from the `forwards.json` ledger.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use super::centered;
use crate::forwards::ForwardEntry;
use crate::state::now_unix;

pub enum ForwardsOutcome {
    Continue,
    Close,
    /// Stop the forward with this id (the app kills the process + drops it from the ledger).
    Kill(String),
}

pub struct ForwardsManager {
    /// A snapshot of the active forwards, refreshed by the app each tick.
    forwards: Vec<ForwardEntry>,
    selected: usize,
    /// Index pending a stop confirmation, if any.
    confirm: Option<usize>,
}

impl ForwardsManager {
    pub fn new(forwards: Vec<ForwardEntry>) -> Self {
        ForwardsManager {
            forwards,
            selected: 0,
            confirm: None,
        }
    }

    /// Replace the snapshot (called each tick after the app reconciles), keeping the selection
    /// and any pending confirmation in range.
    pub fn set_forwards(&mut self, forwards: Vec<ForwardEntry>) {
        self.forwards = forwards;
        if self.selected >= self.forwards.len() {
            self.selected = self.forwards.len().saturating_sub(1);
        }
        if let Some(i) = self.confirm
            && i >= self.forwards.len()
        {
            self.confirm = None;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ForwardsOutcome {
        if self.confirm.is_some() {
            return self.on_confirm_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), true) | (KeyCode::Char('s'), true) => {
                return ForwardsOutcome::Close;
            }
            (KeyCode::Down, false) | (KeyCode::Char('n'), true) => {
                if !self.forwards.is_empty() && self.selected + 1 < self.forwards.len() {
                    self.selected += 1;
                }
            }
            (KeyCode::Up, false) | (KeyCode::Char('p'), true) => {
                self.selected = self.selected.saturating_sub(1);
            }
            (KeyCode::Char('d'), false) | (KeyCode::Char('k'), false)
                if self.selected < self.forwards.len() =>
            {
                self.confirm = Some(self.selected);
            }
            _ => {}
        }
        ForwardsOutcome::Continue
    }

    fn on_confirm_key(&mut self, key: KeyEvent) -> ForwardsOutcome {
        let Some(idx) = self.confirm.take() else {
            return ForwardsOutcome::Continue;
        };
        if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'))
            && let Some(entry) = self.forwards.get(idx)
        {
            return ForwardsOutcome::Kill(entry.id.clone());
        }
        ForwardsOutcome::Continue
    }
}

/// A compact age like `45s`, `12m`, `3h05m` from a start timestamp.
fn age(started_at: i64) -> String {
    let secs = (now_unix() - started_at).max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

pub fn render(frame: &mut Frame, m: &ForwardsManager) {
    let area = centered(
        frame.area(),
        frame.area().width.saturating_sub(6).clamp(50, 92),
        16,
    );
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" port forwards ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let accent = Style::default().fg(super::accent());
    let dim = Style::default().fg(Color::DarkGray);

    let rows = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Min(0),
        ratatui::layout::Constraint::Length(1),
    ])
    .split(inner);

    if m.forwards.is_empty() {
        frame.render_widget(
            Paragraph::new("no active port forwards — open one with ^f on a host").style(dim),
            rows[0],
        );
    } else {
        let host_w = m
            .forwards
            .iter()
            .map(|f| f.host_name.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(6, 20);
        let items: Vec<ListItem> = m
            .forwards
            .iter()
            .map(|f| {
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{:<host_w$}  ", f.host_name)),
                    Span::raw(f.display.clone()),
                    Span::styled(format!("   (pid {} · {})", f.pid, age(f.started_at)), dim),
                ]))
            })
            .collect();
        let list = List::new(items)
            .highlight_symbol("▸ ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default();
        state.select(Some(m.selected.min(m.forwards.len().saturating_sub(1))));
        frame.render_stateful_widget(list, rows[0], &mut state);
    }

    if let Some(idx) = m.confirm {
        let what = m
            .forwards
            .get(idx)
            .map(|f| f.display.as_str())
            .unwrap_or("");
        frame.render_widget(
            Paragraph::new(format!("stop {what}? y = stop · any other key = cancel"))
                .style(Style::default().fg(Color::Red)),
            rows[1],
        );
    } else {
        frame.render_widget(
            Paragraph::new("d/k stop · ↑↓ move · esc close").style(accent),
            rows[1],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forwards::{ForwardKind, ForwardSpec};

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn entry(id: &str, pid: i32) -> ForwardEntry {
        ForwardEntry {
            id: id.into(),
            host_id: "h".into(),
            host_name: "web".into(),
            kind: ForwardKind::Local,
            spec: ForwardSpec {
                bind: None,
                listen_port: 8080,
                target_host: Some("db".into()),
                target_port: Some(3306),
            },
            display: "L  127.0.0.1:8080 → db:3306".into(),
            pid,
            started_at: now_unix(),
        }
    }

    #[test]
    fn kill_needs_confirmation() {
        let mut m = ForwardsManager::new(vec![entry("a", 1), entry("b", 2)]);
        // d opens the confirm; a non-y cancels it with no Kill.
        m.handle_key(k(KeyCode::Char('d')));
        assert!(matches!(
            m.handle_key(k(KeyCode::Char('n'))),
            ForwardsOutcome::Continue
        ));
        // d then y kills the selected (first) entry.
        m.handle_key(k(KeyCode::Char('d')));
        match m.handle_key(k(KeyCode::Char('y'))) {
            ForwardsOutcome::Kill(id) => assert_eq!(id, "a"),
            _ => panic!("expected Kill"),
        }
    }

    #[test]
    fn esc_closes() {
        let mut m = ForwardsManager::new(vec![entry("a", 1)]);
        assert!(matches!(
            m.handle_key(k(KeyCode::Esc)),
            ForwardsOutcome::Close
        ));
    }

    #[test]
    fn set_forwards_clamps_selection() {
        let mut m = ForwardsManager::new(vec![entry("a", 1), entry("b", 2), entry("c", 3)]);
        m.handle_key(k(KeyCode::Down));
        m.handle_key(k(KeyCode::Down)); // selected = 2
        m.set_forwards(vec![entry("a", 1)]); // shrink
        assert_eq!(m.selected, 0);
    }

    #[test]
    fn renders_snapshot() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let m = ForwardsManager::new(vec![entry("a", 4242)]);
        let mut term = Terminal::new(TestBackend::new(72, 10)).unwrap();
        term.draw(|f| render(f, &m)).unwrap();
        let buf = term.backend().buffer();
        let width = buf.area.width as usize;
        let snapshot: String = buf
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(snapshot.contains("port forwards"));
        assert!(snapshot.contains("web"));
        assert!(snapshot.contains("pid 4242"));

        if let Ok(dir) = std::env::var("CARGO_MANIFEST_DIR") {
            let path = std::path::Path::new(&dir).join("target/forwards-manager-snapshot.txt");
            let _ = std::fs::write(path, &snapshot);
        }
    }
}
