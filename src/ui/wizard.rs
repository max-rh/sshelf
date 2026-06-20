//! The add/edit host form (a self-contained component: state + key handling + render).
//!
//! A single full-screen form with focusable fields (Tab / ↑↓ to move, ←/→ to change choosers,
//! Ctrl-s or Enter-on-last-field to save, Esc to cancel). Each field shows a dim placeholder
//! explaining it. The visible fields depend on the chosen Auth method:
//!   - agent:    no extra fields
//!   - key:      Identity (picker over ~/.ssh keys) + optional Key passphrase
//!   - password: Password
//!
//! The optional secret (a login password OR a key passphrase) is stored in the keyring/vault
//! and auto-supplied at connect time; it is never written to `hosts.toml`.

use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::browse::{self, BrowseOutcome, FileBrowser};
use super::centered;
use super::widgets::TextField;
use crate::model::{AuthMethod, Host};

/// Columns before a field's value: marker(2) + padded label(14) + space(1).
const VALUE_COL: u16 = 17;
const LABEL_W: usize = 14;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Hostname,
    User,
    Port,
    Auth,
    Identity,
    Secret,
    JumpHosts,
    Tags,
    Site,
    ExtraArgs,
}

impl Field {
    fn label(self, auth: AuthMethod) -> &'static str {
        match self {
            Field::Name => "Name",
            Field::Hostname => "Hostname",
            Field::User => "User",
            Field::Port => "Port",
            Field::Auth => "Auth",
            Field::Identity => "Key",
            Field::Secret => {
                if auth == AuthMethod::Password {
                    "Password"
                } else {
                    "Key passphrase"
                }
            }
            Field::JumpHosts => "Jump hosts",
            Field::Tags => "Tags",
            Field::Site => "Site",
            Field::ExtraArgs => "Extra args",
        }
    }

    fn placeholder(self, auth: AuthMethod) -> &'static str {
        match self {
            Field::Name => "required · alias you'll search for (e.g. prod-web)",
            Field::Hostname => "required · IP or DNS name (e.g. 10.0.0.5)",
            Field::User => "optional · login user (defaults to $USER)",
            Field::Port => "optional · SSH port (defaults to 22)",
            Field::Auth => "←/→ to change",
            Field::Identity => "optional · ←/→ recent keys · ↵ browse for a key file",
            Field::Secret => {
                if auth == AuthMethod::Password {
                    "optional · stored in keyring/vault — never in hosts.toml"
                } else {
                    "optional · only if the key is encrypted"
                }
            }
            Field::JumpHosts => "optional · ProxyJump chain, e.g. bastion,host2",
            Field::Tags => "optional · labels, space/comma separated, e.g. prod db",
            Field::Site => "←/→ choose a site · (none) = no site · manage with F3",
            Field::ExtraArgs => "optional · extra ssh flags, e.g. -o BatchMode=yes",
        }
    }
}

/// A chooser over discovered SSH private keys (cycled with ←/→).
struct KeyPicker {
    options: Vec<String>,
    idx: usize,
}

impl KeyPicker {
    fn new(discovered: Vec<String>, preselect: Option<&str>) -> Self {
        let mut options = discovered;
        if let Some(p) = preselect
            && !p.is_empty()
            && !options.iter().any(|o| o == p)
        {
            options.insert(0, p.to_string());
        }
        let idx = preselect
            .and_then(|p| options.iter().position(|o| o == p))
            .unwrap_or(0);
        KeyPicker { options, idx }
    }

    fn selected(&self) -> Option<&str> {
        self.options.get(self.idx).map(String::as_str)
    }

    /// Select `p`, adding it to the options if it isn't a discovered key (e.g. a `.pem` the
    /// user browsed to outside `~/.ssh`).
    fn select_path(&mut self, p: &str) {
        match self.options.iter().position(|o| o == p) {
            Some(i) => self.idx = i,
            None => {
                self.options.push(p.to_string());
                self.idx = self.options.len() - 1;
            }
        }
    }

    fn prev(&mut self) {
        if !self.options.is_empty() {
            self.idx = (self.idx + self.options.len() - 1) % self.options.len();
        }
    }
    fn next(&mut self) {
        if !self.options.is_empty() {
            self.idx = (self.idx + 1) % self.options.len();
        }
    }

    /// What to show in the field row.
    fn display(&self) -> String {
        match self.selected() {
            None => "(no keys found in ~/.ssh)".to_string(),
            Some(path) => {
                let name = Path::new(path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(path);
                format!("< {name} >    ({}/{})", self.idx + 1, self.options.len())
            }
        }
    }
}

/// The "no site" sentinel shown first in the [`SitePicker`].
const NO_SITE: &str = "(none)";

/// A chooser over `(none)` + the defined site names (cycled with ←/→).
struct SitePicker {
    options: Vec<String>, // options[0] is the NO_SITE sentinel
    idx: usize,
}

impl SitePicker {
    fn new(site_names: &[String], preselect: Option<&str>) -> Self {
        let mut options = vec![NO_SITE.to_string()];
        options.extend(site_names.iter().cloned());
        // Editing a host whose site isn't (or no longer is) in the defined set: keep it as an
        // option so saving doesn't silently drop the assignment.
        if let Some(p) = preselect
            && !p.is_empty()
            && !options.iter().any(|o| o.eq_ignore_ascii_case(p))
        {
            options.push(p.to_string());
        }
        let idx = preselect
            .filter(|p| !p.is_empty())
            .and_then(|p| options.iter().position(|o| o.eq_ignore_ascii_case(p)))
            .unwrap_or(0);
        SitePicker { options, idx }
    }

    /// The chosen site name, or `None` for "(none)".
    fn selected(&self) -> Option<&str> {
        match self.options.get(self.idx).map(String::as_str) {
            Some(NO_SITE) => None,
            other => other,
        }
    }

    fn prev(&mut self) {
        if !self.options.is_empty() {
            self.idx = (self.idx + self.options.len() - 1) % self.options.len();
        }
    }
    fn next(&mut self) {
        if !self.options.is_empty() {
            self.idx = (self.idx + 1) % self.options.len();
        }
    }

    fn display(&self) -> String {
        let cur = self
            .options
            .get(self.idx)
            .map(String::as_str)
            .unwrap_or(NO_SITE);
        format!("< {cur} >    ({}/{})", self.idx + 1, self.options.len())
    }
}

// Save carries a Host (large) while the others are unit variants; this enum is a transient
// return value, never stored, so the size difference is fine.
#[allow(clippy::large_enum_variant)]
pub enum WizardOutcome {
    Continue,
    Cancel,
    /// Save the host; `secret` is `Some` only when a password / key passphrase was entered
    /// (blank on edit means "keep the existing secret").
    Save {
        host: Host,
        secret: Option<String>,
    },
}

pub struct Wizard {
    editing_id: Option<String>,
    /// Focus index into the currently-active field list.
    focus: usize,
    name: TextField,
    hostname: TextField,
    user: TextField,
    port: TextField,
    auth: AuthMethod,
    identity: KeyPicker,
    /// Identity files beyond the first (the picker edits the first); preserved across edits so
    /// a host configured with multiple keys isn't silently reduced to one.
    extra_identities: Vec<String>,
    secret: TextField,
    jump: TextField,
    tags: TextField,
    site: SitePicker,
    extra: TextField,
    /// File browser modal, open when the user is picking a key file.
    browser: Option<FileBrowser>,
    error: Option<String>,
}

impl Wizard {
    pub fn new_add(site_names: &[String]) -> Self {
        Self::build(
            None,
            AuthMethod::Agent,
            discover_ssh_keys(),
            None,
            site_names,
        )
    }

    pub fn from_host(h: &Host, site_names: &[String]) -> Self {
        let mut w = Self::build(
            Some(h.id.clone()),
            h.auth,
            discover_ssh_keys(),
            h.identity_files.first().map(String::as_str),
            site_names,
        );
        w.name = TextField::with(&h.name);
        w.hostname = TextField::with(&h.hostname);
        w.user = TextField::with(h.user.clone().unwrap_or_default());
        w.port = TextField::with(h.port.map(|p| p.to_string()).unwrap_or_default());
        w.jump = TextField::with(h.jump_hosts.join(", "));
        w.tags = TextField::with(h.tags.join(", "));
        w.site = SitePicker::new(site_names, h.site.as_deref());
        w.extra = TextField::with(h.extra_args.clone().unwrap_or_default());
        w.extra_identities = h.identity_files.iter().skip(1).cloned().collect();
        // secret stays blank on edit (blank = keep existing)
        w
    }

    fn build(
        editing_id: Option<String>,
        auth: AuthMethod,
        keys: Vec<String>,
        preselect: Option<&str>,
        site_names: &[String],
    ) -> Self {
        Wizard {
            editing_id,
            focus: 0,
            name: TextField::new(),
            hostname: TextField::new(),
            user: TextField::new(),
            port: TextField::new(),
            auth,
            identity: KeyPicker::new(keys, preselect),
            extra_identities: Vec::new(),
            secret: TextField::new(),
            jump: TextField::new(),
            tags: TextField::new(),
            site: SitePicker::new(site_names, None),
            extra: TextField::new(),
            browser: None,
            error: None,
        }
    }

    pub fn is_edit(&self) -> bool {
        self.editing_id.is_some()
    }

    /// Fields visible for the current auth method, in display order.
    fn active(&self) -> Vec<Field> {
        let mut v = vec![
            Field::Name,
            Field::Hostname,
            Field::User,
            Field::Port,
            Field::Auth,
        ];
        match self.auth {
            AuthMethod::Key => {
                v.push(Field::Identity);
                v.push(Field::Secret);
            }
            AuthMethod::Password => v.push(Field::Secret),
            AuthMethod::Agent => {}
        }
        v.extend([Field::JumpHosts, Field::Tags, Field::Site, Field::ExtraArgs]);
        v
    }

    fn text_field_mut(&mut self, f: Field) -> Option<&mut TextField> {
        match f {
            Field::Name => Some(&mut self.name),
            Field::Hostname => Some(&mut self.hostname),
            Field::User => Some(&mut self.user),
            Field::Port => Some(&mut self.port),
            Field::Secret => Some(&mut self.secret),
            Field::JumpHosts => Some(&mut self.jump),
            Field::Tags => Some(&mut self.tags),
            Field::ExtraArgs => Some(&mut self.extra),
            Field::Auth | Field::Identity | Field::Site => None,
        }
    }

    fn text_field(&self, f: Field) -> Option<&TextField> {
        match f {
            Field::Name => Some(&self.name),
            Field::Hostname => Some(&self.hostname),
            Field::User => Some(&self.user),
            Field::Port => Some(&self.port),
            Field::Secret => Some(&self.secret),
            Field::JumpHosts => Some(&self.jump),
            Field::Tags => Some(&self.tags),
            Field::ExtraArgs => Some(&self.extra),
            Field::Auth | Field::Identity | Field::Site => None,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> WizardOutcome {
        // While the file browser is open, all keys go to it.
        if let Some(browser) = self.browser.as_mut() {
            match browser.handle_key(key) {
                BrowseOutcome::Continue => {}
                BrowseOutcome::Cancel => self.browser = None,
                BrowseOutcome::Pick(path) => {
                    self.identity.select_path(&path.to_string_lossy());
                    self.browser = None;
                }
            }
            return WizardOutcome::Continue;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('s')) {
            return self.try_save();
        }
        let active = self.active();
        let last = active.len().saturating_sub(1);
        let focused = active[self.focus.min(last)];
        match key.code {
            KeyCode::Esc => return WizardOutcome::Cancel,
            KeyCode::Enter => {
                if focused == Field::Identity {
                    self.open_browser(); // Enter on the Key field browses for a file
                } else if self.focus >= last {
                    return self.try_save();
                } else {
                    self.focus += 1;
                }
            }
            KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % active.len(),
            KeyCode::BackTab | KeyCode::Up => {
                self.focus = (self.focus + active.len() - 1) % active.len()
            }
            code => match focused {
                Field::Auth => match code {
                    KeyCode::Left => self.auth = prev_auth(self.auth),
                    KeyCode::Right | KeyCode::Char(' ') => self.auth = next_auth(self.auth),
                    _ => {}
                },
                Field::Identity => match code {
                    KeyCode::Left => self.identity.prev(),
                    KeyCode::Right | KeyCode::Char(' ') => self.identity.next(),
                    _ => {}
                },
                Field::Site => match code {
                    KeyCode::Left => self.site.prev(),
                    KeyCode::Right | KeyCode::Char(' ') => self.site.next(),
                    _ => {}
                },
                f => {
                    if let Some(tf) = self.text_field_mut(f) {
                        tf.handle(code);
                    }
                }
            },
        }
        // Auth may have changed the active set; keep focus in range.
        let len = self.active().len();
        if self.focus >= len {
            self.focus = len - 1;
        }
        WizardOutcome::Continue
    }

    /// Open the file browser, starting near the current key (or `~/.ssh`, or `$HOME`, or `/`).
    fn open_browser(&mut self) {
        let start = self
            .identity
            .selected()
            .map(Path::new)
            .and_then(Path::parent)
            .filter(|p| p.is_dir())
            .map(Path::to_path_buf)
            .or_else(default_browse_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        self.browser = Some(FileBrowser::new(start));
    }

    fn try_save(&mut self) -> WizardOutcome {
        let name = self.name.value.trim().to_string();
        let hostname = self.hostname.value.trim().to_string();
        if name.is_empty() {
            return self.fail("Name is required", Field::Name);
        }
        if hostname.is_empty() {
            return self.fail("Hostname is required", Field::Hostname);
        }
        let port = {
            let t = self.port.value.trim();
            if t.is_empty() {
                None
            } else {
                match t.parse::<u16>() {
                    Ok(p) => Some(p),
                    Err(_) => return self.fail("Port must be a number 1-65535", Field::Port),
                }
            }
        };

        let mut h = Host::new(name, hostname);
        if let Some(id) = &self.editing_id {
            h.id = id.clone();
        }
        let user = self.user.value.trim();
        h.user = (!user.is_empty()).then(|| user.to_string());
        h.port = port;
        h.auth = self.auth;
        h.identity_files = if self.auth == AuthMethod::Key {
            // The picker chooses the primary key; preserve any extra keys from the original
            // host so editing doesn't silently drop a multi-key configuration.
            let mut ids: Vec<String> = self
                .identity
                .selected()
                .map(|s| vec![s.to_string()])
                .unwrap_or_default();
            for extra in &self.extra_identities {
                if !ids.contains(extra) {
                    ids.push(extra.clone());
                }
            }
            ids
        } else {
            Vec::new()
        };
        h.jump_hosts = split_list(&self.jump.value);
        h.tags = split_tags(&self.tags.value);
        h.site = self.site.selected().map(str::to_string);
        let extra = self.extra.value.trim();
        h.extra_args = (!extra.is_empty()).then(|| extra.to_string());

        // The optional secret applies to password auth (login password) and key auth (key
        // passphrase). For agent auth there is no secret.
        let secret = if matches!(self.auth, AuthMethod::Password | AuthMethod::Key) {
            let s = self.secret.value.trim();
            (!s.is_empty()).then(|| s.to_string())
        } else {
            None
        };

        WizardOutcome::Save { host: h, secret }
    }

    fn fail(&mut self, msg: &str, field: Field) -> WizardOutcome {
        self.error = Some(msg.to_string());
        if let Some(pos) = self.active().iter().position(|f| *f == field) {
            self.focus = pos;
        }
        WizardOutcome::Continue
    }
}

fn next_auth(a: AuthMethod) -> AuthMethod {
    match a {
        AuthMethod::Agent => AuthMethod::Key,
        AuthMethod::Key => AuthMethod::Password,
        AuthMethod::Password => AuthMethod::Agent,
    }
}
fn prev_auth(a: AuthMethod) -> AuthMethod {
    next_auth(next_auth(a))
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}
fn split_tags(s: &str) -> Vec<String> {
    s.split([',', ' ', '\t'])
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

/// `~/.ssh` (if present) else `$HOME` — where the file browser starts.
fn default_browse_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let ssh = home.join(".ssh");
    Some(if ssh.is_dir() { ssh } else { home })
}

/// Private keys under `~/.ssh`: anything that has a `<name>.pub` sibling **or** whose header
/// says "PRIVATE KEY" (so `.pem` and keyless OpenSSH keys are found too). For keys elsewhere,
/// the user browses to them via the file picker.
fn discover_ssh_keys() -> Vec<String> {
    match std::env::var_os("HOME") {
        Some(home) => scan_keys(&PathBuf::from(home).join(".ssh")),
        None => Vec::new(),
    }
}

/// Sorted private-key paths found directly in `dir` (testable; `discover_ssh_keys` calls it
/// with `~/.ssh`).
fn scan_keys(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| p.extension().and_then(|s| s.to_str()) != Some("pub"))
        .filter(|p| is_private_key(p))
        // We can only store UTF-8 paths in hosts.toml; skip (rare) non-UTF8 names.
        .filter_map(|p| p.to_str().map(str::to_owned))
        .collect();
    keys.sort();
    keys
}

fn is_private_key(path: &Path) -> bool {
    // A `<name>.pub` sibling (checked without a lossy conversion so non-UTF8 names match).
    let mut pubp = path.to_path_buf().into_os_string();
    pubp.push(".pub");
    if Path::new(&pubp).exists() {
        return true;
    }
    // Otherwise sniff the header (covers .pem and keys whose .pub was removed).
    use std::io::Read;
    let mut head = [0u8; 64];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    looks_like_private_key(&head[..n])
}

/// The opening bytes of a PEM/OpenSSH private key contain "PRIVATE KEY"
/// (`-----BEGIN [RSA|EC|OPENSSH] PRIVATE KEY-----`), which `.pub`/config/known_hosts never do.
fn looks_like_private_key(head: &[u8]) -> bool {
    String::from_utf8_lossy(head).contains("PRIVATE KEY")
}

pub fn render(frame: &mut Frame, w: &Wizard) {
    let active = w.active();
    // Size to the terminal (with a margin), capped for readability, so placeholders fit.
    let width = frame.area().width.saturating_sub(6).clamp(56, 100);
    let area = centered(frame.area(), width, active.len() as u16 + 7);
    frame.render_widget(Clear, area);

    let title = if w.is_edit() {
        " edit host "
    } else {
        " add host "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let accent = Style::default()
        .fg(super::accent())
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    for (row, &field) in active.iter().enumerate() {
        let focused = row == w.focus;
        let marker = if focused { "▸ " } else { "  " };
        let label_style = if focused { accent } else { Style::default() };

        // value span (+ whether it's a placeholder so we can dim it)
        let (value, is_placeholder) = field_value(w, field);
        let value_style = if is_placeholder {
            dim
        } else {
            Style::default()
        };

        let line = Line::from(vec![
            Span::styled(marker, accent),
            Span::styled(format!("{:<LABEL_W$} ", field.label(w.auth)), label_style),
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
    if let Some(err) = &w.error {
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
    let hint = if active.get(w.focus) == Some(&Field::Identity) {
        "Tab/↑↓ move · ←/→ recent keys · ↵ browse files · ^s save · esc cancel"
    } else {
        "Tab/↑↓ move · ←/→ change · ↵ next (saves on last) · ^s save · esc cancel"
    };
    frame.render_widget(
        Paragraph::new(hint).style(dim),
        Rect {
            x: inner.x,
            y: y + 1,
            width: inner.width,
            height: 1,
        },
    );

    // The file browser modal draws on top of the form (and owns the screen while open).
    if let Some(b) = &w.browser {
        browse::render(frame, b);
        return;
    }

    // Terminal cursor on the focused *text* field (not the choosers).
    if let Some(&field) = active.get(w.focus)
        && let Some(tf) = w.text_field(field)
    {
        let cx = inner.x + VALUE_COL + tf.cursor as u16;
        let cx = cx.min(inner.x + inner.width.saturating_sub(1));
        frame.set_cursor_position((cx, inner.y + w.focus as u16));
    }
}

/// (display string, is_placeholder) for a field.
fn field_value(w: &Wizard, field: Field) -> (String, bool) {
    match field {
        Field::Auth => (
            format!("< {} >    (key · agent · password)", w.auth.as_str()),
            false,
        ),
        Field::Identity => (w.identity.display(), w.identity.selected().is_none()),
        Field::Site => (w.site.display(), w.site.selected().is_none()),
        Field::Secret => {
            let n = w.secret.value.chars().count();
            if n == 0 {
                (field.placeholder(w.auth).to_string(), true)
            } else {
                ("•".repeat(n), false)
            }
        }
        _ => {
            let tf = w.text_field(field).expect("text field");
            if tf.value.is_empty() {
                (field.placeholder(w.auth).to_string(), true)
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

    fn with_keys(keys: &[&str]) -> Wizard {
        Wizard::build(
            None,
            AuthMethod::Agent,
            keys.iter().map(|s| s.to_string()).collect(),
            None,
            &[],
        )
    }

    /// Move focus to a field by repeatedly Tab-ing (active set may change with auth).
    fn goto(w: &mut Wizard, field: Field) {
        for _ in 0..40 {
            if w.active().get(w.focus) == Some(&field) {
                return;
            }
            w.focus = (w.focus + 1) % w.active().len();
        }
        panic!("field not reachable");
    }

    fn type_str(w: &mut Wizard, s: &str) {
        for c in s.chars() {
            w.handle_key(k(KeyCode::Char(c)));
        }
    }

    #[test]
    fn agent_auth_hides_identity_and_secret() {
        let w = with_keys(&[]);
        let active = w.active();
        assert!(!active.contains(&Field::Identity));
        assert!(!active.contains(&Field::Secret));
    }

    #[test]
    fn key_auth_shows_identity_and_passphrase() {
        let mut w = with_keys(&["/home/u/.ssh/id_ed25519"]);
        w.auth = AuthMethod::Key;
        let active = w.active();
        assert!(active.contains(&Field::Identity));
        assert!(active.contains(&Field::Secret));
    }

    #[test]
    fn password_auth_shows_only_password_secret() {
        let mut w = with_keys(&["/k"]);
        w.auth = AuthMethod::Password;
        let active = w.active();
        assert!(!active.contains(&Field::Identity));
        assert!(active.contains(&Field::Secret));
    }

    #[test]
    fn key_picker_selects_and_saves_identity() {
        let mut w = with_keys(&["/home/u/.ssh/id_ed25519", "/home/u/.ssh/id_rsa"]);
        w.auth = AuthMethod::Key;
        type_str(&mut w, "n");
        goto(&mut w, Field::Hostname);
        type_str(&mut w, "h");
        goto(&mut w, Field::Identity);
        w.handle_key(k(KeyCode::Right)); // pick id_rsa
        match w.try_save() {
            WizardOutcome::Save { host, .. } => {
                assert_eq!(host.identity_files, vec!["/home/u/.ssh/id_rsa".to_string()]);
            }
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn key_passphrase_is_captured_as_secret() {
        let mut w = with_keys(&["/k"]);
        w.auth = AuthMethod::Key;
        type_str(&mut w, "n");
        goto(&mut w, Field::Hostname);
        type_str(&mut w, "h");
        goto(&mut w, Field::Secret);
        type_str(&mut w, "keypass");
        match w.try_save() {
            WizardOutcome::Save { host, secret } => {
                assert_eq!(host.auth, AuthMethod::Key);
                assert_eq!(secret.as_deref(), Some("keypass"));
            }
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn agent_auth_drops_any_secret() {
        let mut w = with_keys(&[]);
        type_str(&mut w, "n");
        goto(&mut w, Field::Hostname);
        type_str(&mut w, "h");
        // secret field isn't active for agent; force a stray value and confirm it's dropped
        w.secret = TextField::with("ignored");
        match w.try_save() {
            WizardOutcome::Save { secret, .. } => assert!(secret.is_none()),
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn site_chooser_selects_and_saves() {
        let sites = vec!["prod-dc".to_string(), "staging".to_string()];
        let mut w = Wizard::build(None, AuthMethod::Agent, vec![], None, &sites);
        type_str(&mut w, "n");
        goto(&mut w, Field::Hostname);
        type_str(&mut w, "h");
        goto(&mut w, Field::Site);
        w.handle_key(k(KeyCode::Right)); // (none) -> prod-dc
        match w.try_save() {
            WizardOutcome::Save { host, .. } => assert_eq!(host.site.as_deref(), Some("prod-dc")),
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn from_host_preselects_the_site() {
        let sites = vec!["prod-dc".to_string()];
        let mut h = Host::new("web", "h");
        h.site = Some("prod-dc".into());
        let w = Wizard::from_host(&h, &sites);
        assert_eq!(w.site.selected(), Some("prod-dc"));
    }

    #[test]
    fn requires_name_and_hostname() {
        let mut w = with_keys(&[]);
        assert!(matches!(w.try_save(), WizardOutcome::Continue));
        assert!(w.error.is_some());
    }

    #[test]
    fn auth_cycles_and_keeps_focus_valid() {
        let mut w = with_keys(&["/k"]);
        goto(&mut w, Field::Auth);
        let auth_pos = w.focus;
        w.handle_key(k(KeyCode::Right)); // agent -> key (adds fields)
        assert_eq!(w.auth, AuthMethod::Key);
        assert_eq!(w.focus, auth_pos); // still on Auth
        assert!(w.focus < w.active().len());
    }

    #[test]
    fn edit_preserves_extra_identity_files() {
        let mut h = Host::new("multi", "host");
        h.auth = AuthMethod::Key;
        h.identity_files = vec!["/keys/a".into(), "/keys/b".into()];
        let mut w = Wizard::from_host(&h, &[]);
        // Don't touch the picker; just save.
        match w.try_save() {
            WizardOutcome::Save { host, .. } => {
                assert_eq!(
                    host.identity_files,
                    vec!["/keys/a".to_string(), "/keys/b".to_string()]
                );
            }
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn edit_preserves_id() {
        let mut h = Host::new("old", "host");
        h.user = Some("me".into());
        let mut w = Wizard::from_host(&h, &[]);
        match w.try_save() {
            WizardOutcome::Save { host, .. } => {
                assert_eq!(host.id, h.id);
                assert_eq!(host.user.as_deref(), Some("me"));
            }
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn enter_on_key_field_opens_browser() {
        let mut w = with_keys(&["/k"]);
        w.auth = AuthMethod::Key;
        goto(&mut w, Field::Identity);
        assert!(w.browser.is_none());
        w.handle_key(k(KeyCode::Enter));
        assert!(
            w.browser.is_some(),
            "Enter on the Key field should open the browser"
        );
        // keys now route to the browser; Esc closes it without affecting the form
        w.handle_key(k(KeyCode::Esc));
        assert!(w.browser.is_none());
    }

    #[test]
    fn scan_keys_finds_pem_and_pairs_skips_others() {
        let dir = std::env::temp_dir().join(format!("sshelf-scan-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        // a .pem with NO .pub sibling (the AWS case)
        std::fs::write(dir.join("aws.pem"), b"-----BEGIN RSA PRIVATE KEY-----\n").unwrap();
        // a normal keypair
        std::fs::write(
            dir.join("id_ed25519"),
            b"-----BEGIN OPENSSH PRIVATE KEY-----\n",
        )
        .unwrap();
        std::fs::write(dir.join("id_ed25519.pub"), b"ssh-ed25519 AAAA...\n").unwrap();
        // non-keys that must be excluded
        std::fs::write(dir.join("known_hosts"), b"github.com ssh-ed25519 AAAA\n").unwrap();
        std::fs::write(dir.join("config"), b"Host x\n  HostName y\n").unwrap();

        let keys = scan_keys(&dir);
        let names: Vec<String> = keys
            .iter()
            .map(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert!(names.contains(&"aws.pem".to_string()), "got {names:?}");
        assert!(names.contains(&"id_ed25519".to_string()), "got {names:?}");
        assert!(
            !names
                .iter()
                .any(|n| n == "known_hosts" || n == "config" || n.ends_with(".pub"))
        );
    }

    #[test]
    fn detects_private_key_header() {
        assert!(looks_like_private_key(b"-----BEGIN RSA PRIVATE KEY-----\n"));
        assert!(looks_like_private_key(
            b"-----BEGIN OPENSSH PRIVATE KEY-----"
        ));
        assert!(!looks_like_private_key(b"ssh-ed25519 AAAAC3Nza qwe"));
        assert!(!looks_like_private_key(b""));
    }

    #[test]
    fn select_path_adds_unknown_key() {
        let mut w = with_keys(&["/home/u/.ssh/id_ed25519"]);
        w.auth = AuthMethod::Key;
        w.identity.select_path("/downloads/aws.pem");
        assert_eq!(w.identity.selected(), Some("/downloads/aws.pem"));
        type_str(&mut w, "n");
        goto(&mut w, Field::Hostname);
        type_str(&mut w, "h");
        match w.try_save() {
            WizardOutcome::Save { host, .. } => {
                assert_eq!(host.identity_files, vec!["/downloads/aws.pem".to_string()]);
            }
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn renders_and_writes_snapshot() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut w = with_keys(&["/home/u/.ssh/infra-key", "/home/u/.ssh/id_ed25519"]);
        w.auth = AuthMethod::Key;
        w.name = TextField::with("prod-db");
        w.hostname = TextField::with("10.25.25.25");
        w.user = TextField::with("mike");
        w.tags = TextField::with("prod, db");

        let mut term = Terminal::new(TestBackend::new(74, 20)).unwrap();
        term.draw(|f| render(f, &w)).unwrap();
        let buf = term.backend().buffer();
        let width = buf.area.width as usize;
        let lines: Vec<String> = buf
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect();
        let snapshot = lines.join("\n");

        assert!(snapshot.contains("add host"));
        assert!(snapshot.contains("Key"));
        assert!(snapshot.contains("Key passphrase"));

        if let Ok(dir) = std::env::var("CARGO_MANIFEST_DIR") {
            let p = std::path::Path::new(&dir).join("target/wizard-snapshot.txt");
            let _ = std::fs::write(p, &snapshot);
        }
    }
}
