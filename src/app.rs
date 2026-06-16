//! Application state, key handling, and the synchronous event loop.
//!
//! `on_key` is pure (mutates `App`, returns an `Outcome`) so it can be unit-tested by
//! feeding synthetic key events. The event loop is a thin wrapper around it.
//!
//! Connecting is deferred until *after* the terminal is restored: `on_key` returns
//! `Outcome::Connect`, the loop records it and quits, and `run` performs the `exec()` handoff
//! once the TUI is torn down (so ssh inherits a clean TTY).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::config::Config;
use crate::import;
use crate::model::{CURRENT_FORMAT_VERSION, Host, HostsFile};
use crate::paths::Paths;
use crate::search;
use crate::secrets;
use crate::ssh;
use crate::state::FrecencyState;
use crate::store;
use crate::transfer::{self, TransferOutcome};
use crate::ui;
use crate::ui::settings::{Settings, SettingsOutcome};
use crate::ui::wizard::{Wizard, WizardOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    List,
    Help,
}

/// Pending delete confirmation.
pub struct ConfirmDelete {
    pub id: String,
    pub name: String,
}

/// What the event loop should do after handling a key. Variants carry an index into
/// `App::hosts`.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Quit,
    Connect(usize),
    Yank(usize),
    /// Open the transfer screen for this host (the loop spawns the worker after key handling).
    Transfer(usize),
}

pub struct App {
    pub hosts: Vec<Host>,
    pub state: FrecencyState,
    pub config: Config,
    pub paths: Paths,
    /// Resolved host-database path (config override or default).
    pub hosts_path: PathBuf,
    /// Live search query.
    pub query: String,
    /// Host indices (into `hosts`) in display order.
    pub order: Vec<usize>,
    /// Selected position within `order`.
    pub selected: usize,
    pub screen: Screen,
    pub wizard: Option<Wizard>,
    pub confirm: Option<ConfirmDelete>,
    pub settings: Option<Settings>,
    /// The dual-pane file-transfer screen, when open.
    pub transfer: Option<transfer::TransferScreen>,
    /// Transient status line (cleared on next keypress).
    pub status: Option<String>,
    pub should_quit: bool,
    /// Set when the user chose a host; the real connect happens after terminal restore.
    pub pending_connect: Option<usize>,
}

impl App {
    pub fn new(hosts: Vec<Host>, state: FrecencyState, config: Config, paths: Paths) -> Self {
        let hosts_path = config.hosts_path(&paths);
        let mut app = App {
            hosts,
            state,
            config,
            paths,
            hosts_path,
            query: String::new(),
            order: Vec::new(),
            selected: 0,
            screen: Screen::List,
            wizard: None,
            confirm: None,
            settings: None,
            transfer: None,
            status: None,
            should_quit: false,
            pending_connect: None,
        };
        app.recompute();
        app
    }

    /// Re-rank the host list for the current query and clamp the selection.
    pub fn recompute(&mut self) {
        self.order = search::rank(
            &self.hosts,
            &self.query,
            &self.state,
            self.config.decay_rate,
            self.config.default_sort,
        );
        if self.selected >= self.order.len() {
            self.selected = self.order.len().saturating_sub(1);
        }
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some(msg.into());
    }

    /// The host index currently under the cursor, if any.
    fn current(&self) -> Option<usize> {
        self.order.get(self.selected).copied()
    }

    fn persist_hosts(&self) -> Result<()> {
        let file = HostsFile {
            format_version: CURRENT_FORMAT_VERSION,
            hosts: self.hosts.clone(),
        };
        store::save_hosts(&self.hosts_path, &file)
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Outcome {
        if key.kind != KeyEventKind::Press {
            return Outcome::Continue;
        }
        if let Some(screen) = self.transfer.as_mut() {
            // Dropping the screen closes the worker (master + socket torn down) via its RAII.
            if let TransferOutcome::Close = screen.on_key(key) {
                self.transfer = None;
            }
            return Outcome::Continue;
        }
        if self.confirm.is_some() {
            self.on_key_confirm(key);
            return Outcome::Continue;
        }
        if self.wizard.is_some() {
            self.on_key_wizard(key);
            return Outcome::Continue;
        }
        if self.settings.is_some() {
            self.on_key_settings(key);
            return Outcome::Continue;
        }
        match self.screen {
            Screen::Help => {
                self.screen = Screen::List;
                Outcome::Continue
            }
            Screen::List => self.on_key_list(key),
        }
    }

    fn on_key_list(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        self.status = None;
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) => return Outcome::Quit,
            (KeyCode::Esc, _) => {
                if self.query.is_empty() {
                    return Outcome::Quit;
                }
                self.query.clear();
                self.selected = 0;
                self.recompute();
            }
            (KeyCode::Enter, _) => {
                if let Some(i) = self.current() {
                    return Outcome::Connect(i);
                }
            }
            (KeyCode::Char('y'), true) => {
                if let Some(i) = self.current() {
                    return Outcome::Yank(i);
                }
            }
            (KeyCode::Char('t'), true) => {
                if let Some(i) = self.current() {
                    return Outcome::Transfer(i);
                }
            }
            (KeyCode::Char('a'), true) => self.wizard = Some(Wizard::new_add()),
            (KeyCode::Char('e'), true) => match self.current() {
                Some(i) => self.wizard = Some(Wizard::from_host(&self.hosts[i])),
                None => self.set_status("no host selected"),
            },
            (KeyCode::Char('d'), true) => {
                if let Some(i) = self.current() {
                    let h = &self.hosts[i];
                    self.confirm = Some(ConfirmDelete {
                        id: h.id.clone(),
                        name: h.name.clone(),
                    });
                }
            }
            (KeyCode::Char('o'), true) => self.import_ssh_config(),
            (KeyCode::Down, false) | (KeyCode::Char('n'), true) => self.move_down(),
            (KeyCode::Up, false) | (KeyCode::Char('p'), true) => self.move_up(),
            (KeyCode::F(1), _) => self.screen = Screen::Help,
            (KeyCode::F(2), _) => self.open_settings(),
            (KeyCode::Backspace, _) => {
                self.query.pop();
                self.selected = 0;
                self.recompute();
            }
            // Plain printable characters extend the query (type-to-filter).
            (KeyCode::Char(c), false) => {
                self.query.push(c);
                self.selected = 0;
                self.recompute();
            }
            _ => {}
        }
        Outcome::Continue
    }

    fn on_key_wizard(&mut self, key: KeyEvent) {
        let outcome = match self.wizard.as_mut() {
            Some(w) => w.handle_key(key),
            None => return,
        };
        match outcome {
            WizardOutcome::Continue => {}
            WizardOutcome::Cancel => self.wizard = None,
            WizardOutcome::Save { host, secret } => {
                let id = host.id.clone();
                let updated = match self.hosts.iter().position(|h| h.id == host.id) {
                    Some(pos) => {
                        self.hosts[pos] = host;
                        true
                    }
                    None => {
                        self.hosts.push(host);
                        false
                    }
                };
                self.wizard = None;
                let secret_err = match secret {
                    Some(pw) => secrets::store_password(&self.paths.vault_file(), &id, &pw).err(),
                    None => None,
                };
                match self.persist_hosts() {
                    Ok(()) => match secret_err {
                        Some(e) => self.set_status(format!("host saved; secret NOT stored: {e}")),
                        None => self.set_status(if updated {
                            "host updated"
                        } else {
                            "host added"
                        }),
                    },
                    Err(e) => self.set_status(format!("save failed: {e}")),
                }
                self.recompute();
            }
        }
    }

    fn open_settings(&mut self) {
        self.settings = Some(Settings::new(
            self.paths.config_file().display().to_string(),
            self.config.hosts_file.clone(),
            self.paths.default_hosts_display(),
        ));
    }

    fn on_key_settings(&mut self, key: KeyEvent) {
        let outcome = match self.settings.as_mut() {
            Some(s) => s.handle_key(key),
            None => return,
        };
        match outcome {
            SettingsOutcome::Continue => {}
            SettingsOutcome::Cancel => self.settings = None,
            SettingsOutcome::Save { hosts_file } => {
                self.settings = None;
                // Resolve the proposed path WITHOUT committing config yet.
                let proposed = Config {
                    hosts_file: hosts_file.clone(),
                    ..self.config.clone()
                };
                let new_path = proposed.hosts_path(&self.paths);

                if new_path == self.hosts_path {
                    self.config.hosts_file = hosts_file;
                    match self.config.save(&self.paths.config_file()) {
                        Ok(()) => self.set_status("settings saved"),
                        Err(e) => self.set_status(format!("could not save config: {e}")),
                    }
                    return;
                }

                // Handle the database at the new location. Adopt an existing file (never
                // overwrite it); otherwise write the current hosts there so they follow.
                let outcome: std::result::Result<String, String> = if new_path.exists() {
                    match store::load_hosts(&new_path) {
                        Ok(file) => {
                            self.hosts = file.hosts;
                            Ok(format!("using existing hosts at {}", new_path.display()))
                        }
                        Err(e) => Err(format!("could not read {}: {e}", new_path.display())),
                    }
                } else {
                    let file = HostsFile {
                        format_version: CURRENT_FORMAT_VERSION,
                        hosts: self.hosts.clone(),
                    };
                    match store::save_hosts(&new_path, &file) {
                        Ok(()) => Ok(format!("hosts moved to {}", new_path.display())),
                        Err(e) => Err(format!("hosts NOT written: {e}")),
                    }
                };

                match outcome {
                    Ok(msg) => {
                        // Commit config only now that the hosts step succeeded.
                        self.config.hosts_file = hosts_file;
                        self.hosts_path = new_path;
                        self.recompute();
                        match self.config.save(&self.paths.config_file()) {
                            Ok(()) => self.set_status(format!("settings saved · {msg}")),
                            Err(e) => {
                                self.set_status(format!("hosts updated; config NOT saved: {e}"))
                            }
                        }
                    }
                    Err(e) => self.set_status(format!("settings not applied · {e}")),
                }
            }
        }
    }

    fn on_key_confirm(&mut self, key: KeyEvent) {
        let delete = matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
        let confirm = self.confirm.take();
        if !delete {
            return;
        }
        if let Some(c) = confirm {
            self.hosts.retain(|h| h.id != c.id);
            self.state.stats.remove(&c.id);
            let _ = self.state.save(&self.paths.state_file());
            let _ = secrets::delete_password(&self.paths.vault_file(), &c.id);
            match self.persist_hosts() {
                Ok(()) => self.set_status(format!("deleted {}", c.name)),
                Err(e) => self.set_status(format!("save failed: {e}")),
            }
            self.recompute();
        }
    }

    /// Import new hosts from ~/.ssh/config (read-only), skipping names we already have.
    fn import_ssh_config(&mut self) {
        let path = match import::default_config_path() {
            Some(p) if p.exists() => p,
            Some(p) => {
                self.set_status(format!("no ssh config at {}", p.display()));
                return;
            }
            None => {
                self.set_status("HOME is not set");
                return;
            }
        };
        match import::parse_file(&path) {
            Ok(result) => {
                let to_add: Vec<Host> = import::new_hosts(&result.hosts, &self.hosts)
                    .into_iter()
                    .cloned()
                    .collect();
                let added = to_add.len();
                let total = result.hosts.len();
                self.hosts.extend(to_add);
                let persisted = self.persist_hosts();
                self.recompute();
                let warn = if result.warnings.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", result.warnings.join("; "))
                };
                match persisted {
                    Ok(()) => {
                        self.set_status(format!("imported {added} new of {total} host(s){warn}"))
                    }
                    Err(e) => self.set_status(format!("parsed ok but save failed: {e}")),
                }
            }
            Err(e) => self.set_status(format!("import failed: {e}")),
        }
    }

    /// Spawn the transfer worker for `idx` and open the dual-pane screen. Mirrors connect: the
    /// event loop calls this so the side effects (a worker thread, a secrets lookup) stay out
    /// of the testable `on_key`.
    fn open_transfer(&mut self, idx: usize) {
        let host = self.hosts[idx].clone();
        let has_secret = secrets::get_password(&self.paths.vault_file(), &host.id)
            .ok()
            .flatten()
            .is_some();
        let start = std::env::current_dir()
            .ok()
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"));
        match transfer::TransferScreen::open(&host, has_secret, start) {
            Ok(screen) => self.transfer = Some(screen),
            Err(e) => self.set_status(format!("could not start transfer: {e}")),
        }
    }

    fn move_down(&mut self) {
        if !self.order.is_empty() && self.selected + 1 < self.order.len() {
            self.selected += 1;
        }
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

/// Set up the terminal, run the loop, restore the terminal, then (if a host was chosen)
/// perform the `exec()` handoff into ssh.
pub fn run() -> Result<()> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let _ = Config::ensure_default_file(&paths.config_file()); // best-effort
    let config = Config::load(&paths.config_file())?;
    let hosts = store::load_hosts(&config.hosts_path(&paths))?.hosts;
    let state = FrecencyState::load(&paths.state_file())?;
    let mut app = App::new(hosts, state, config, paths);

    let mut terminal = ratatui::init();
    let loop_result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    loop_result?;

    if let Some(idx) = app.pending_connect {
        let host = app.hosts[idx].clone();
        // Persist usage BEFORE exec() — nothing runs after a successful exec.
        app.state.record_use(&host.id);
        if let Err(e) = app.state.save(&app.paths.state_file()) {
            eprintln!("sshelf: warning: could not save state: {e:#}");
        }
        // Wire SSH_ASKPASS only when a secret is actually stored (login password OR key
        // passphrase). Otherwise let ssh prompt / use the agent normally.
        let has_secret = secrets::get_password(&app.paths.vault_file(), &host.id)
            .ok()
            .flatten()
            .is_some();
        // Replaces this process on success; returns only on failure.
        return Err(ssh::exec_connect(&host, has_secret));
    }
    Ok(())
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| ui::render(frame, app))?;
        if app.transfer.is_some() {
            // The transfer screen is live: poll so worker events and progress animate without a
            // keypress, then drain whatever the worker produced this tick.
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
            {
                dispatch(app, key);
            }
            if let Some(screen) = app.transfer.as_mut() {
                screen.drain_events();
            }
        } else if let Event::Key(key) = event::read()? {
            dispatch(app, key);
        }
    }
    Ok(())
}

/// Handle one key by routing the resulting outcome (shared by the blocking and polling paths).
fn dispatch(app: &mut App, key: KeyEvent) {
    match app.on_key(key) {
        Outcome::Quit => app.should_quit = true,
        Outcome::Connect(idx) => {
            app.pending_connect = Some(idx);
            app.should_quit = true;
        }
        Outcome::Yank(idx) => {
            let cmd = ssh::command_string(&app.hosts[idx]);
            if ssh::copy_to_clipboard(&cmd) {
                app.set_status(format!("copied: {cmd}"));
            } else {
                app.set_status(cmd);
            }
        }
        Outcome::Transfer(idx) => app.open_transfer(idx),
        Outcome::Continue => {}
    }
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

    fn test_app() -> App {
        let hosts = vec![
            Host::new("prod-web", "10.0.0.1"),
            Host::new("prod-db", "10.0.0.2"),
            Host::new("bastion", "bastion.example.com"),
        ];
        // Use a unique temp dir so persistence side-effects don't collide between tests.
        let dir = std::env::temp_dir().join(format!("sshelf-app-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths {
            config_dir: dir.clone(),
            data_dir: dir,
            config_file_override: None,
        };
        App::new(hosts, FrecencyState::default(), Config::default(), paths)
    }

    #[test]
    fn typing_filters_and_resets_selection() {
        let mut app = test_app();
        app.selected = 2;
        for c in "prod".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.query, "prod");
        assert_eq!(app.selected, 0);
        assert_eq!(app.order.len(), 2);
    }

    #[test]
    fn esc_clears_then_quits() {
        let mut app = test_app();
        app.on_key(key(KeyCode::Char('p')));
        assert!(!app.query.is_empty());
        assert_eq!(app.on_key(key(KeyCode::Esc)), Outcome::Continue);
        assert!(app.query.is_empty());
        assert_eq!(app.on_key(key(KeyCode::Esc)), Outcome::Quit);
    }

    #[test]
    fn ctrl_c_quits() {
        let mut app = test_app();
        assert_eq!(app.on_key(ctrl(KeyCode::Char('c'))), Outcome::Quit);
    }

    #[test]
    fn enter_connects_to_selected() {
        let mut app = test_app();
        app.move_down();
        let expected = app.order[app.selected];
        assert_eq!(app.on_key(key(KeyCode::Enter)), Outcome::Connect(expected));
    }

    #[test]
    fn ctrl_y_yanks_selected() {
        let mut app = test_app();
        let expected = app.order[app.selected];
        assert_eq!(
            app.on_key(ctrl(KeyCode::Char('y'))),
            Outcome::Yank(expected)
        );
    }

    #[test]
    fn ctrl_t_opens_transfer_for_selected() {
        let mut app = test_app();
        let expected = app.order[app.selected];
        assert_eq!(
            app.on_key(ctrl(KeyCode::Char('t'))),
            Outcome::Transfer(expected)
        );
    }

    #[test]
    fn navigation_is_bounded() {
        let mut app = test_app();
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.selected, 0);
        for _ in 0..10 {
            app.on_key(key(KeyCode::Down));
        }
        assert_eq!(app.selected, app.order.len() - 1);
    }

    #[test]
    fn f1_toggles_help() {
        let mut app = test_app();
        app.on_key(key(KeyCode::F(1)));
        assert_eq!(app.screen, Screen::Help);
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(app.screen, Screen::List);
    }

    #[test]
    fn ctrl_a_opens_add_wizard() {
        let mut app = test_app();
        app.on_key(ctrl(KeyCode::Char('a')));
        assert!(app.wizard.is_some());
        // typing now goes to the wizard, not the query
        app.on_key(key(KeyCode::Char('z')));
        assert!(app.query.is_empty());
    }

    #[test]
    fn add_host_via_wizard_persists() {
        let mut app = test_app();
        let before = app.hosts.len();
        app.on_key(ctrl(KeyCode::Char('a')));
        for c in "newbox".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Tab));
        for c in "192.0.2.5".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(ctrl(KeyCode::Char('s'))); // save
        assert!(app.wizard.is_none());
        assert_eq!(app.hosts.len(), before + 1);
        assert!(app.hosts.iter().any(|h| h.name == "newbox"));
        // and it was written to disk
        let reloaded = store::load_hosts(&app.hosts_path).unwrap();
        assert!(reloaded.hosts.iter().any(|h| h.name == "newbox"));
    }

    #[test]
    fn delete_confirm_removes_host() {
        let mut app = test_app();
        let target = app.order[app.selected];
        let id = app.hosts[target].id.clone();
        app.on_key(ctrl(KeyCode::Char('d')));
        assert!(app.confirm.is_some());
        app.on_key(key(KeyCode::Char('y')));
        assert!(app.confirm.is_none());
        assert!(!app.hosts.iter().any(|h| h.id == id));
    }

    #[test]
    fn delete_cancelled_keeps_host() {
        let mut app = test_app();
        let before = app.hosts.len();
        app.on_key(ctrl(KeyCode::Char('d')));
        app.on_key(key(KeyCode::Char('n')));
        assert!(app.confirm.is_none());
        assert_eq!(app.hosts.len(), before);
    }

    fn open_settings_set_hosts_path(app: &mut App, path: &std::path::Path) {
        app.on_key(key(KeyCode::F(2)));
        assert!(app.settings.is_some());
        for c in path.to_string_lossy().chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(ctrl(KeyCode::Char('s')));
        assert!(app.settings.is_none());
    }

    #[test]
    fn settings_adopts_existing_hosts_file_without_overwriting() {
        let mut app = test_app();
        // An existing host DB at the new location (the user's "backup").
        let dir = std::env::temp_dir().join(format!("sshelf-adopt-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let new_path = dir.join("hosts.toml");
        let existing = HostsFile {
            format_version: CURRENT_FORMAT_VERSION,
            hosts: vec![Host::new("fromdisk", "9.9.9.9")],
        };
        store::save_hosts(&new_path, &existing).unwrap();

        open_settings_set_hosts_path(&mut app, &new_path);

        assert_eq!(app.hosts_path, new_path);
        assert_eq!(app.hosts.len(), 1);
        assert!(app.hosts.iter().any(|h| h.name == "fromdisk"));
        // The existing file must be UNCHANGED (adopted, not overwritten).
        let reloaded = store::load_hosts(&new_path).unwrap();
        assert_eq!(reloaded.hosts.len(), 1);
        assert_eq!(
            app.config.hosts_file.as_deref(),
            Some(new_path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn settings_writes_hosts_to_new_location() {
        let mut app = test_app();
        let before = app.hosts.len();
        let dir = std::env::temp_dir().join(format!("sshelf-newloc-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let new_path = dir.join("hosts.toml");
        assert!(!new_path.exists());

        open_settings_set_hosts_path(&mut app, &new_path);

        assert_eq!(app.hosts_path, new_path);
        assert!(new_path.exists());
        let reloaded = store::load_hosts(&new_path).unwrap();
        assert_eq!(reloaded.hosts.len(), before);
    }
}
