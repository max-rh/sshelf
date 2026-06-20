//! The sites manager (F3): create / edit / delete sites and their optional shared SSH defaults.
//!
//! Two levels: a **list** of sites, and an inline **form** for one site (name + optional
//! user/port/jump/identity). On save it returns the edited site list plus the name **renames**
//! made while editing; the app applies those to member hosts and clears any host whose site no
//! longer exists (so deletes self-heal).

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use super::centered;
use super::widgets::TextField;
use crate::model::Site;

/// Form fields, in display order: (label, placeholder).
const FORM_FIELDS: [(&str, &str); 5] = [
    ("Name", "required · the name hosts reference"),
    ("User", "optional · default login for member hosts"),
    ("Port", "optional · default SSH port"),
    (
        "Jump",
        "optional · default ProxyJump (bastion), comma-separated",
    ),
    (
        "Identity",
        "optional · default key file(s), comma-separated",
    ),
];
const LABEL_W: usize = 10;
/// Columns before a field value: marker(2) + padded label + space(1).
const VALUE_COL: u16 = 2 + LABEL_W as u16 + 1;

pub enum SitesOutcome {
    Continue,
    Cancel,
    /// Commit the edited site list. The app applies `renames` (old → new) to member hosts, then
    /// clears any host whose site name is no longer defined.
    Save {
        sites: Vec<Site>,
        renames: Vec<(String, String)>,
    },
}

// `Form` carries a SiteForm (several TextFields) while the others are tiny; this is transient
// per-manager state, never stored in bulk, so the size difference is fine.
#[allow(clippy::large_enum_variant)]
enum Mode {
    List,
    Form(SiteForm),
    ConfirmDelete(usize),
}

pub struct SitesManager {
    sites: Vec<Site>,
    selected: usize,
    mode: Mode,
    /// Name changes made while editing, applied to member hosts on save.
    renames: Vec<(String, String)>,
}

impl SitesManager {
    pub fn new(sites: Vec<Site>) -> Self {
        SitesManager {
            sites,
            selected: 0,
            mode: Mode::List,
            renames: Vec::new(),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SitesOutcome {
        match self.mode {
            Mode::List => self.on_list_key(key),
            Mode::ConfirmDelete(_) => self.on_confirm_key(key),
            Mode::Form(_) => self.on_form_key(key),
        }
    }

    fn on_list_key(&mut self, key: KeyEvent) -> SitesOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), true) => return SitesOutcome::Cancel,
            (KeyCode::Char('s'), true) => {
                return SitesOutcome::Save {
                    sites: std::mem::take(&mut self.sites),
                    renames: std::mem::take(&mut self.renames),
                };
            }
            (KeyCode::Down, false) | (KeyCode::Char('n'), true) => {
                if !self.sites.is_empty() && self.selected + 1 < self.sites.len() {
                    self.selected += 1;
                }
            }
            (KeyCode::Up, false) | (KeyCode::Char('p'), true) => {
                self.selected = self.selected.saturating_sub(1);
            }
            (KeyCode::Char('a'), false) => self.mode = Mode::Form(SiteForm::new_add()),
            (KeyCode::Char('e'), false) | (KeyCode::Enter, _) => {
                if let Some(s) = self.sites.get(self.selected) {
                    self.mode = Mode::Form(SiteForm::edit(self.selected, s));
                }
            }
            (KeyCode::Char('d'), false) if self.selected < self.sites.len() => {
                self.mode = Mode::ConfirmDelete(self.selected);
            }
            _ => {}
        }
        SitesOutcome::Continue
    }

    fn on_confirm_key(&mut self, key: KeyEvent) -> SitesOutcome {
        let Mode::ConfirmDelete(idx) = self.mode else {
            return SitesOutcome::Continue;
        };
        if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) && idx < self.sites.len() {
            self.sites.remove(idx); // member hosts are orphan-cleared by the app on save
            if self.selected >= self.sites.len() {
                self.selected = self.sites.len().saturating_sub(1);
            }
        }
        self.mode = Mode::List;
        SitesOutcome::Continue
    }

    fn on_form_key(&mut self, key: KeyEvent) -> SitesOutcome {
        // Take the form out so we can freely mutate `self` while applying it.
        let Mode::Form(mut form) = std::mem::replace(&mut self.mode, Mode::List) else {
            return SitesOutcome::Continue;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc {
            return SitesOutcome::Continue; // discard; back to the list
        }
        let last = FORM_FIELDS.len() - 1;
        let commit = (ctrl && key.code == KeyCode::Char('s'))
            || (key.code == KeyCode::Enter && form.focus >= last);
        if commit {
            match form.build(&self.sites) {
                Ok(site) => {
                    self.apply_form(&form, site);
                    return SitesOutcome::Continue; // mode already List
                }
                Err(e) => {
                    form.error = Some(e);
                    self.mode = Mode::Form(form);
                    return SitesOutcome::Continue;
                }
            }
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down => form.focus = (form.focus + 1) % FORM_FIELDS.len(),
            KeyCode::BackTab | KeyCode::Up => {
                form.focus = (form.focus + FORM_FIELDS.len() - 1) % FORM_FIELDS.len()
            }
            KeyCode::Enter => form.focus += 1, // not last (commit handled that)
            code => {
                form.field_mut(form.focus).handle(code);
            }
        }
        self.mode = Mode::Form(form);
        SitesOutcome::Continue
    }

    /// Store a built site back into the list (recording a rename if its name changed on edit).
    fn apply_form(&mut self, form: &SiteForm, site: Site) {
        match form.editing {
            Some(idx) => {
                if let Some(orig) = &form.original_name
                    && !orig.eq_ignore_ascii_case(&site.name)
                {
                    self.renames.push((orig.clone(), site.name.clone()));
                }
                if idx < self.sites.len() {
                    self.sites[idx] = site;
                    self.selected = idx;
                }
            }
            None => {
                self.sites.push(site);
                self.selected = self.sites.len() - 1;
            }
        }
    }
}

struct SiteForm {
    /// Index being edited, or `None` for a new site.
    editing: Option<usize>,
    original_name: Option<String>,
    focus: usize,
    fields: [TextField; 5],
    error: Option<String>,
}

impl SiteForm {
    fn new_add() -> Self {
        SiteForm {
            editing: None,
            original_name: None,
            focus: 0,
            fields: std::array::from_fn(|_| TextField::new()),
            error: None,
        }
    }

    fn edit(idx: usize, s: &Site) -> Self {
        SiteForm {
            editing: Some(idx),
            original_name: Some(s.name.clone()),
            focus: 0,
            fields: [
                TextField::with(&s.name),
                TextField::with(s.user.clone().unwrap_or_default()),
                TextField::with(s.port.map(|p| p.to_string()).unwrap_or_default()),
                TextField::with(s.jump_hosts.join(", ")),
                TextField::with(s.identity_files.join(", ")),
            ],
            error: None,
        }
    }

    fn field(&self, i: usize) -> &TextField {
        &self.fields[i]
    }
    fn field_mut(&mut self, i: usize) -> &mut TextField {
        &mut self.fields[i]
    }

    /// Validate + build a `Site`. `existing` is the working list (to reject a duplicate name).
    fn build(&self, existing: &[Site]) -> Result<Site, String> {
        let name = self.fields[0].value.trim().to_string();
        if name.is_empty() {
            return Err("Name is required".into());
        }
        let dup = existing
            .iter()
            .enumerate()
            .any(|(i, s)| Some(i) != self.editing && s.name.eq_ignore_ascii_case(&name));
        if dup {
            return Err(format!("a site named '{name}' already exists"));
        }
        let port = {
            let t = self.fields[2].value.trim();
            if t.is_empty() {
                None
            } else {
                Some(
                    t.parse::<u16>()
                        .map_err(|_| "Port must be a number 1-65535".to_string())?,
                )
            }
        };
        Ok(Site {
            name,
            user: opt(&self.fields[1].value),
            port,
            jump_hosts: split_list(&self.fields[3].value),
            identity_files: split_list(&self.fields[4].value),
        })
    }
}

fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

/// A one-line summary of a site's defaults (dim when it carries none).
fn summary(s: &Site) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(u) = &s.user {
        parts.push(format!("user {u}"));
    }
    if let Some(p) = s.port {
        parts.push(format!("port {p}"));
    }
    if !s.jump_hosts.is_empty() {
        parts.push(format!("-J {}", s.jump_hosts.join(",")));
    }
    if !s.identity_files.is_empty() {
        parts.push(format!("-i {}", s.identity_files.len()));
    }
    if parts.is_empty() {
        "(grouping only)".to_string()
    } else {
        parts.join("  ")
    }
}

pub fn render(frame: &mut Frame, m: &SitesManager) {
    let area = centered(
        frame.area(),
        frame.area().width.saturating_sub(6).clamp(50, 90),
        18,
    );
    frame.render_widget(Clear, area);
    let block = Block::default().borders(Borders::ALL).title(" sites ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match &m.mode {
        Mode::Form(form) => render_form(frame, inner, form),
        _ => render_list(frame, inner, m),
    }
}

fn render_list(frame: &mut Frame, area: Rect, m: &SitesManager) {
    let accent = Style::default().fg(super::accent());
    let dim = Style::default().fg(Color::DarkGray);

    let rows = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Min(0),
        ratatui::layout::Constraint::Length(1),
    ])
    .split(area);

    if m.sites.is_empty() {
        frame.render_widget(
            Paragraph::new("no sites yet — press a to add one").style(dim),
            rows[0],
        );
    } else {
        let name_w = m
            .sites
            .iter()
            .map(|s| s.name.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(6, 20);
        let items: Vec<ListItem> = m
            .sites
            .iter()
            .map(|s| {
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{:<name_w$}  ", s.name)),
                    Span::styled(summary(s), dim),
                ]))
            })
            .collect();
        let list = List::new(items)
            .highlight_symbol("▸ ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default();
        state.select(Some(m.selected.min(m.sites.len().saturating_sub(1))));
        frame.render_stateful_widget(list, rows[0], &mut state);
    }

    let hint = match &m.mode {
        Mode::ConfirmDelete(idx) => {
            let name = m.sites.get(*idx).map(|s| s.name.as_str()).unwrap_or("");
            return frame.render_widget(
                Paragraph::new(format!(
                    "delete '{name}'? y = delete · any other key = cancel"
                ))
                .style(Style::default().fg(Color::Red)),
                rows[1],
            );
        }
        _ => "a add · e/↵ edit · d delete · ^s save · esc cancel",
    };
    frame.render_widget(Paragraph::new(hint).style(accent), rows[1]);
}

fn render_form(frame: &mut Frame, area: Rect, form: &SiteForm) {
    let accent = Style::default()
        .fg(super::accent())
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    for (row, (label, placeholder)) in FORM_FIELDS.iter().enumerate() {
        let focused = row == form.focus;
        let marker = if focused { "▸ " } else { "  " };
        let label_style = if focused { accent } else { Style::default() };
        let tf = form.field(row);
        let (value, is_placeholder) = if tf.value.is_empty() {
            ((*placeholder).to_string(), true)
        } else {
            (tf.value.clone(), false)
        };
        let value_style = if is_placeholder {
            dim
        } else {
            Style::default()
        };
        let line = Line::from(vec![
            Span::styled(marker, accent),
            Span::styled(format!("{label:<LABEL_W$} "), label_style),
            Span::styled(value, value_style),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect {
                x: area.x,
                y: area.y + row as u16,
                width: area.width,
                height: 1,
            },
        );
    }

    let y = area.y + FORM_FIELDS.len() as u16 + 1;
    if let Some(err) = &form.error {
        frame.render_widget(
            Paragraph::new(format!("⚠ {err}")).style(Style::default().fg(Color::Red)),
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            },
        );
    }
    frame.render_widget(
        Paragraph::new("Tab/↑↓ move · ↵ next (saves on last) · ^s save · esc back").style(dim),
        Rect {
            x: area.x,
            y: y + 1,
            width: area.width,
            height: 1,
        },
    );

    // Cursor on the focused field.
    let tf = form.field(form.focus);
    let cx = area.x + VALUE_COL + tf.cursor as u16;
    let cx = cx.min(area.x + area.width.saturating_sub(1));
    frame.set_cursor_position((cx, area.y + form.focus as u16));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }
    fn type_str(m: &mut SitesManager, s: &str) {
        for c in s.chars() {
            m.handle_key(k(KeyCode::Char(c)));
        }
    }

    #[test]
    fn add_a_site_with_defaults() {
        let mut m = SitesManager::new(vec![]);
        m.handle_key(k(KeyCode::Char('a'))); // open the add form (Name focused)
        type_str(&mut m, "prod-dc");
        m.handle_key(k(KeyCode::Down)); // → User
        type_str(&mut m, "deploy");
        m.handle_key(ctrl(KeyCode::Char('s'))); // commit the form
        match m.handle_key(ctrl(KeyCode::Char('s'))) {
            // save the manager
            SitesOutcome::Save { sites, renames } => {
                assert_eq!(sites.len(), 1);
                assert_eq!(sites[0].name, "prod-dc");
                assert_eq!(sites[0].user.as_deref(), Some("deploy"));
                assert!(renames.is_empty());
            }
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn editing_the_name_records_a_rename() {
        let mut m = SitesManager::new(vec![Site::new("old")]);
        m.handle_key(k(KeyCode::Enter)); // edit selected (Name focused, value "old")
        // clear "old" and type "new"
        for _ in 0..3 {
            m.handle_key(k(KeyCode::Backspace));
        }
        type_str(&mut m, "new");
        m.handle_key(ctrl(KeyCode::Char('s'))); // commit
        match m.handle_key(ctrl(KeyCode::Char('s'))) {
            SitesOutcome::Save { sites, renames } => {
                assert_eq!(sites[0].name, "new");
                assert_eq!(renames, vec![("old".to_string(), "new".to_string())]);
            }
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn delete_needs_confirmation() {
        let mut m = SitesManager::new(vec![Site::new("a"), Site::new("b")]);
        m.handle_key(k(KeyCode::Char('d'))); // confirm prompt for "a"
        m.handle_key(k(KeyCode::Char('n'))); // any non-y cancels
        assert_eq!(m.sites.len(), 2);
        m.handle_key(k(KeyCode::Char('d')));
        m.handle_key(k(KeyCode::Char('y'))); // confirm
        assert_eq!(m.sites.len(), 1);
        assert_eq!(m.sites[0].name, "b");
    }

    #[test]
    fn rejects_a_duplicate_name() {
        let mut m = SitesManager::new(vec![Site::new("dup")]);
        m.handle_key(k(KeyCode::Char('a')));
        type_str(&mut m, "DUP"); // case-insensitive clash
        m.handle_key(ctrl(KeyCode::Char('s')));
        // The form stays open with an error; the site list is unchanged.
        assert!(matches!(m.mode, Mode::Form(_)));
        assert_eq!(m.sites.len(), 1);
    }

    #[test]
    fn esc_from_list_cancels() {
        let mut m = SitesManager::new(vec![Site::new("a")]);
        assert!(matches!(
            m.handle_key(k(KeyCode::Esc)),
            SitesOutcome::Cancel
        ));
    }
}
