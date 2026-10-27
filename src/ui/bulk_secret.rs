//! The bulk-credentials popup (`^k`): give many hosts a password in one go, or point them at
//! one shared [`Credential`](crate::model::Credential) so a single stored secret serves all of
//! them and rotates in one place.
//!
//! Two deliberate safety choices, both visible in the popup rather than buried:
//!
//! - **Existing secrets are skipped by default.** Overwriting one is unrecoverable (the old
//!   value is simply gone), and a bulk action is exactly where that would go unnoticed, so the
//!   count of hosts that already have a secret is always on screen and replacing them is a
//!   toggle the user turns on.
//! - **Nothing is verified here.** Proving N secrets would mean N logins, so the popup stores
//!   and says so; a wrong one surfaces on that host's next connect with the usual refusal
//!   message (D-035). The one-host path (first connect) still checks, because it can.
//!
//! The typed password is a live secret, so the field is masked like the wizard's, travels in a
//! `Zeroizing<String>`, and is wiped when the popup closes.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use zeroize::{Zeroize, Zeroizing};

use super::centered;
use super::widgets::TextField;
use crate::model::validate_credential_name;

/// What the popup will do to the selected hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Store the same password separately for each host, under its own id.
    Each,
    /// Point every host at one named credential, optionally setting its secret.
    Shared,
    /// Give the hosts their own secret again. Leaves the credential and its value alone.
    Unbind,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Each => "one password, stored per host",
            Mode::Shared => "shared credential (one secret for all)",
            Mode::Unbind => "unbind (back to per-host secrets)",
        }
    }

    fn next(self) -> Self {
        match self {
            Mode::Each => Mode::Shared,
            Mode::Shared => Mode::Unbind,
            Mode::Unbind => Mode::Each,
        }
    }
}

/// Which field has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Mode,
    Name,
    Secret,
}

pub enum BulkOutcome {
    Continue,
    Cancel,
    Apply(BulkRequest),
}

/// A validated request the app applies. The secret is `None` when the user left the field
/// empty, which for [`Mode::Shared`] means "bind only, keep whatever is stored".
pub struct BulkRequest {
    pub mode: Mode,
    /// Lower-cased, validated credential name. Always set for [`Mode::Shared`].
    pub credential: Option<String>,
    pub secret: Option<Zeroizing<String>>,
    /// Replace secrets that are already stored, rather than skipping those hosts.
    pub replace_existing: bool,
}

pub struct BulkSecretPopup {
    /// Host indices this applies to, resolved by the caller (marked hosts, else what the
    /// filter shows).
    targets: Vec<usize>,
    /// How many of `targets` already have a secret stored.
    with_secret: usize,
    /// True when the targets came from marks rather than the filter, for the title.
    from_marks: bool,
    mode: Mode,
    name: TextField,
    secret: TextField,
    focus: Field,
    replace_existing: bool,
    error: Option<String>,
}

impl BulkSecretPopup {
    pub fn new(targets: Vec<usize>, with_secret: usize, from_marks: bool) -> Self {
        BulkSecretPopup {
            targets,
            with_secret,
            from_marks,
            mode: Mode::Each,
            name: TextField::new(),
            secret: TextField::new(),
            focus: Field::Mode,
            replace_existing: false,
            error: None,
        }
    }

    pub fn targets(&self) -> &[usize] {
        &self.targets
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> BulkOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c')) {
            return BulkOutcome::Cancel;
        }
        // Toggling "replace existing" needs a key that is not a character, or it would type
        // into the focused field.
        if ctrl && matches!(key.code, KeyCode::Char('r')) {
            self.replace_existing = !self.replace_existing;
            return BulkOutcome::Continue;
        }
        self.error = None;
        match key.code {
            KeyCode::Esc => return BulkOutcome::Cancel,
            KeyCode::Tab | KeyCode::Down => self.focus = self.next_focus(),
            KeyCode::BackTab | KeyCode::Up => self.focus = self.prev_focus(),
            KeyCode::Left if self.focus == Field::Mode => self.mode = self.mode.next().next(),
            KeyCode::Right | KeyCode::Char(' ') if self.focus == Field::Mode => {
                self.mode = self.mode.next();
            }
            KeyCode::Enter => return self.submit(),
            code => {
                match self.focus {
                    Field::Name => {
                        self.name.handle(code);
                    }
                    Field::Secret => {
                        self.secret.handle(code);
                    }
                    Field::Mode => {}
                };
            }
        }
        BulkOutcome::Continue
    }

    /// Fields the current mode actually shows, in order.
    fn fields(&self) -> Vec<Field> {
        match self.mode {
            Mode::Each => vec![Field::Mode, Field::Secret],
            Mode::Shared => vec![Field::Mode, Field::Name, Field::Secret],
            Mode::Unbind => vec![Field::Mode],
        }
    }

    fn next_focus(&self) -> Field {
        let fields = self.fields();
        let at = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        fields[(at + 1) % fields.len()]
    }

    fn prev_focus(&self) -> Field {
        let fields = self.fields();
        let at = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        fields[(at + fields.len() - 1) % fields.len()]
    }

    fn submit(&mut self) -> BulkOutcome {
        if self.targets.is_empty() {
            self.error = Some("no hosts selected".into());
            return BulkOutcome::Continue;
        }
        let secret = {
            let typed = self.secret.value.trim_end_matches(['\n', '\r']);
            (!typed.is_empty()).then(|| Zeroizing::new(typed.to_string()))
        };
        match self.mode {
            Mode::Each => {
                if secret.is_none() {
                    self.error = Some("type the password to store, or esc to cancel".into());
                    return BulkOutcome::Continue;
                }
                BulkOutcome::Apply(BulkRequest {
                    mode: Mode::Each,
                    credential: None,
                    secret,
                    replace_existing: self.replace_existing,
                })
            }
            Mode::Shared => match validate_credential_name(&self.name.value) {
                Ok(name) => BulkOutcome::Apply(BulkRequest {
                    mode: Mode::Shared,
                    credential: Some(name),
                    secret,
                    replace_existing: self.replace_existing,
                }),
                Err(e) => {
                    self.error = Some(e);
                    self.focus = Field::Name;
                    BulkOutcome::Continue
                }
            },
            Mode::Unbind => BulkOutcome::Apply(BulkRequest {
                mode: Mode::Unbind,
                credential: None,
                secret: None,
                replace_existing: false,
            }),
        }
    }
}

impl Drop for BulkSecretPopup {
    fn drop(&mut self) {
        self.secret.value.zeroize();
    }
}

pub fn render(frame: &mut Frame, p: &BulkSecretPopup) {
    let width = frame.area().width.saturating_sub(6).clamp(52, 80);
    let height = match p.mode {
        Mode::Each => 10,
        Mode::Shared => 11,
        Mode::Unbind => 9,
    };
    let area = centered(frame.area(), width, height);
    frame.render_widget(Clear, area);
    let scope = if p.from_marks { "marked" } else { "shown" };
    let block = Block::default().borders(Borders::ALL).title(format!(
        " credentials · {} host(s) {scope} ",
        p.targets.len()
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let dim = Style::default().fg(Color::DarkGray);
    let accent = Style::default()
        .fg(super::accent())
        .add_modifier(Modifier::BOLD);
    let focused = |is: bool| if is { accent } else { dim };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("what:  ", focused(p.focus == Field::Mode)),
        Span::raw(p.mode.label()),
        Span::styled("   (← →)", dim),
    ]));
    if p.mode == Mode::Shared {
        let name = if p.name.value.is_empty() {
            Span::styled("a name, e.g. corp-ldap", dim)
        } else {
            Span::raw(p.name.value.clone())
        };
        lines.push(Line::from(vec![
            Span::styled("name:  ", focused(p.focus == Field::Name)),
            name,
        ]));
    }
    if p.mode != Mode::Unbind {
        let secret = if p.secret.value.is_empty() {
            Span::styled(
                if p.mode == Mode::Shared {
                    "leave empty to bind without changing the stored secret"
                } else {
                    "the password to store for each host"
                },
                dim,
            )
        } else {
            Span::raw("•".repeat(p.secret.value.chars().count()))
        };
        lines.push(Line::from(vec![
            Span::styled("pass:  ", focused(p.focus == Field::Secret)),
            secret,
        ]));
    }
    lines.push(Line::from(""));
    if p.with_secret > 0 && p.mode == Mode::Each {
        let state = if p.replace_existing {
            Span::styled(
                format!("replacing the {} that already have one", p.with_secret),
                Style::default().fg(Color::Yellow),
            )
        } else {
            Span::styled(
                format!("skipping the {} that already have one", p.with_secret),
                dim,
            )
        };
        lines.push(Line::from(vec![state, Span::styled("   (^r)", dim)]));
    }
    if p.mode == Mode::Shared {
        lines.push(Line::from(Span::styled(
            "every host here will authenticate from this one secret",
            dim,
        )));
    }
    if p.mode == Mode::Unbind {
        lines.push(Line::from(Span::styled(
            "the credential itself is kept; these hosts just stop using it",
            dim,
        )));
    }
    if p.mode != Mode::Unbind {
        lines.push(Line::from(Span::styled(
            "not checked against the hosts — a wrong one shows up on the next connect",
            dim,
        )));
    }

    if let Some(err) = &p.error {
        lines.push(Line::from(Span::styled(
            format!("⚠ {err}"),
            Style::default().fg(Color::Red),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height.saturating_sub(1),
        },
    );
    frame.render_widget(
        Paragraph::new("↵ apply · tab next field · ^r replace existing · esc cancel").style(dim),
        Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn typed(p: &mut BulkSecretPopup, s: &str) {
        for c in s.chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn each_mode_needs_a_password() {
        let mut p = BulkSecretPopup::new(vec![0, 1], 0, true);
        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            BulkOutcome::Continue
        ));
        assert!(p.error.is_some());
    }

    #[test]
    fn each_mode_applies_the_typed_password() {
        let mut p = BulkSecretPopup::new(vec![0, 1], 1, true);
        p.handle_key(key(KeyCode::Tab)); // focus the password
        typed(&mut p, "hunter2");
        match p.handle_key(key(KeyCode::Enter)) {
            BulkOutcome::Apply(req) => {
                assert_eq!(req.mode, Mode::Each);
                assert_eq!(req.secret.as_deref().map(String::as_str), Some("hunter2"));
                assert!(!req.replace_existing);
                assert!(req.credential.is_none());
            }
            _ => panic!("expected apply"),
        }
    }

    #[test]
    fn replace_existing_toggles_without_typing_into_the_field() {
        let mut p = BulkSecretPopup::new(vec![0], 1, true);
        p.handle_key(key(KeyCode::Tab));
        p.handle_key(ctrl(KeyCode::Char('r')));
        typed(&mut p, "x");
        match p.handle_key(key(KeyCode::Enter)) {
            BulkOutcome::Apply(req) => {
                assert!(req.replace_existing);
                assert_eq!(req.secret.as_deref().map(String::as_str), Some("x"));
            }
            _ => panic!("expected apply"),
        }
    }

    #[test]
    fn shared_mode_validates_the_name_and_allows_an_empty_secret() {
        let mut p = BulkSecretPopup::new(vec![0, 1, 2], 0, false);
        p.handle_key(key(KeyCode::Right)); // Each -> Shared
        assert_eq!(p.mode, Mode::Shared);
        p.handle_key(key(KeyCode::Tab)); // name
        typed(&mut p, "Corp LDAP");
        assert!(matches!(
            p.handle_key(key(KeyCode::Enter)),
            BulkOutcome::Continue
        ));
        assert!(p.error.is_some(), "a space is not a valid name");
        for _ in 0..9 {
            p.name.handle(KeyCode::Backspace);
        }
        typed(&mut p, "corp-ldap");
        match p.handle_key(key(KeyCode::Enter)) {
            BulkOutcome::Apply(req) => {
                assert_eq!(req.credential.as_deref(), Some("corp-ldap"));
                assert!(req.secret.is_none(), "an empty field binds only");
            }
            _ => panic!("expected apply"),
        }
    }

    #[test]
    fn a_name_is_lowercased_so_the_store_key_is_predictable() {
        let mut p = BulkSecretPopup::new(vec![0], 0, true);
        p.handle_key(key(KeyCode::Right));
        p.handle_key(key(KeyCode::Tab));
        typed(&mut p, "CORP-LDAP");
        match p.handle_key(key(KeyCode::Enter)) {
            BulkOutcome::Apply(req) => assert_eq!(req.credential.as_deref(), Some("corp-ldap")),
            _ => panic!("expected apply"),
        }
    }

    #[test]
    fn unbind_needs_nothing_else() {
        let mut p = BulkSecretPopup::new(vec![0, 1], 2, true);
        p.handle_key(key(KeyCode::Right));
        p.handle_key(key(KeyCode::Right));
        assert_eq!(p.mode, Mode::Unbind);
        match p.handle_key(key(KeyCode::Enter)) {
            BulkOutcome::Apply(req) => {
                assert_eq!(req.mode, Mode::Unbind);
                assert!(req.secret.is_none());
            }
            _ => panic!("expected apply"),
        }
    }

    #[test]
    fn esc_cancels_and_the_mode_cycles_both_ways() {
        let mut p = BulkSecretPopup::new(vec![0], 0, true);
        p.handle_key(key(KeyCode::Left));
        assert_eq!(p.mode, Mode::Unbind, "left wraps backwards");
        assert!(matches!(
            p.handle_key(key(KeyCode::Esc)),
            BulkOutcome::Cancel
        ));
    }
}
