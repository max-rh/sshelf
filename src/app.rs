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
use crate::model::{CURRENT_FORMAT_VERSION, Host, HostsFile, Site};
use crate::paths::Paths;
use crate::search;
use crate::secrets;
use crate::ssh;
use crate::state::FrecencyState;
use crate::store;
use crate::transfer::{self, TransferOutcome};
use crate::ui;
use crate::ui::settings::{Settings, SettingsOutcome};
use crate::ui::sites::{SitesManager, SitesOutcome};
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
    /// Defined sites (groups + optional inherited SSH defaults).
    pub sites: Vec<Site>,
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
    /// The sites manager (F3), when open.
    pub sites_manager: Option<SitesManager>,
    /// The dual-pane file-transfer screen, when open.
    pub transfer: Option<transfer::TransferScreen>,
    /// Transient status line (cleared on next keypress).
    pub status: Option<String>,
    pub should_quit: bool,
    /// Set when the user chose a host; the real connect happens after terminal restore.
    pub pending_connect: Option<usize>,
}

impl App {
    pub fn new(
        hosts: Vec<Host>,
        sites: Vec<Site>,
        state: FrecencyState,
        config: Config,
        paths: Paths,
    ) -> Self {
        let hosts_path = config.hosts_path(&paths);
        let mut app = App {
            hosts,
            sites,
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
            sites_manager: None,
            transfer: None,
            status: None,
            should_quit: false,
            pending_connect: None,
        };
        app.recompute();
        app
    }

    /// Re-rank the host list for the current query and clamp the selection. When the query is
    /// empty (idle) the order is grouped into site sections; while filtering it's the flat
    /// ranked order (`order` always holds host indices — section headers are render-only).
    pub fn recompute(&mut self) {
        let ranked = search::rank(
            &self.hosts,
            &self.query,
            &self.state,
            self.config.decay_rate,
            self.config.default_sort,
        );
        self.order = if self.query.is_empty() {
            group_order(&self.hosts, &ranked)
        } else {
            ranked
        };
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

    /// The defined site names, for the wizard's site chooser.
    fn site_names(&self) -> Vec<String> {
        self.sites.iter().map(|s| s.name.clone()).collect()
    }

    fn persist_hosts(&self) -> Result<()> {
        let file = HostsFile {
            format_version: CURRENT_FORMAT_VERSION,
            sites: self.sites.clone(),
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
        if self.sites_manager.is_some() {
            self.on_key_sites(key);
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
            (KeyCode::Char('a'), true) => {
                let names = self.site_names();
                self.wizard = Some(Wizard::new_add(&names));
            }
            (KeyCode::Char('e'), true) => match self.current() {
                Some(i) => {
                    let names = self.site_names();
                    self.wizard = Some(Wizard::from_host(&self.hosts[i], &names));
                }
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
            (KeyCode::F(3), _) => self.open_sites(),
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

    fn open_sites(&mut self) {
        self.sites_manager = Some(SitesManager::new(self.sites.clone()));
    }

    fn on_key_sites(&mut self, key: KeyEvent) {
        let outcome = match self.sites_manager.as_mut() {
            Some(m) => m.handle_key(key),
            None => return,
        };
        match outcome {
            SitesOutcome::Continue => {}
            SitesOutcome::Cancel => self.sites_manager = None,
            SitesOutcome::Save { sites, renames } => {
                self.sites_manager = None;
                // Apply name changes to member hosts, then clear any host whose site no longer
                // exists (so deletes self-heal — see decisions.md D-020).
                for (old, new) in &renames {
                    for h in &mut self.hosts {
                        if h.site
                            .as_deref()
                            .is_some_and(|s| s.eq_ignore_ascii_case(old))
                        {
                            h.site = Some(new.clone());
                        }
                    }
                }
                let defined: std::collections::HashSet<String> =
                    sites.iter().map(|s| s.name.to_lowercase()).collect();
                for h in &mut self.hosts {
                    if let Some(s) = &h.site
                        && !defined.contains(&s.to_lowercase())
                    {
                        h.site = None;
                    }
                }
                self.sites = sites;
                match self.persist_hosts() {
                    Ok(()) => self.set_status("sites saved"),
                    Err(e) => self.set_status(format!("save failed: {e}")),
                }
                self.recompute();
            }
        }
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
                            self.sites = file.sites;
                            Ok(format!("using existing hosts at {}", new_path.display()))
                        }
                        Err(e) => Err(format!("could not read {}: {e}", new_path.display())),
                    }
                } else {
                    let file = HostsFile {
                        format_version: CURRENT_FORMAT_VERSION,
                        sites: self.sites.clone(),
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
        // Resolve the host's site defaults so transfers ride the site's bastion / use its
        // default user; `id` is preserved so the secrets lookup below is unaffected.
        let host = self.hosts[idx].with_site_defaults(&self.sites);
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

/// Reorder ranked host indices into site sections: distinct sites in case-insensitive name
/// order, the "(no site)" group last; within a section the hosts keep their ranked
/// (frecency/name) order. Returns host indices only — the renderer adds the section headers.
fn group_order(hosts: &[Host], ranked: &[usize]) -> Vec<usize> {
    let mut keys: Vec<String> = ranked
        .iter()
        .filter_map(|&i| hosts[i].site.as_deref().map(str::to_lowercase))
        .collect();
    keys.sort();
    keys.dedup();
    let mut out = Vec::with_capacity(ranked.len());
    for key in &keys {
        out.extend(ranked.iter().copied().filter(|&i| {
            hosts[i]
                .site
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(key))
        }));
    }
    out.extend(ranked.iter().copied().filter(|&i| hosts[i].site.is_none()));
    out
}

/// Set up the terminal, run the loop, restore the terminal, then (if a host was chosen)
/// perform the `exec()` handoff into ssh.
pub fn run() -> Result<()> {
    run_with(false)
}

/// Like [`run`], but with the add-host form already open (`sshelf add`).
pub fn run_add() -> Result<()> {
    run_with(true)
}

fn run_with(start_add: bool) -> Result<()> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let _ = Config::ensure_default_file(&paths.config_file()); // best-effort
    let config = Config::load(&paths.config_file())?;
    let file = store::load_hosts(&config.hosts_path(&paths))?;
    let state = FrecencyState::load(&paths.state_file())?;
    let mut app = App::new(file.hosts, file.sites, state, config, paths);
    if start_add {
        let names = app.site_names();
        app.wizard = Some(Wizard::new_add(&names));
    }

    let mut terminal = ratatui::init();
    let loop_result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    loop_result?;

    if let Some(idx) = app.pending_connect {
        // Resolve the host's site defaults (bastion/user/port/identity) for the real connect.
        let host = app.hosts[idx].with_site_defaults(&app.sites);
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
            let cmd = ssh::command_string(&app.hosts[idx].with_site_defaults(&app.sites));
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
        App::new(
            hosts,
            Vec::new(),
            FrecencyState::default(),
            Config::default(),
            paths,
        )
    }

    #[test]
    fn group_order_sections_sites_alpha_then_no_site() {
        let mut a = Host::new("a", "h");
        a.site = Some("zeta".into());
        let mut b = Host::new("b", "h");
        b.site = Some("alpha".into());
        let mut c = Host::new("c", "h");
        c.site = Some("ALPHA".into()); // same section as b (case-insensitive)
        let d = Host::new("d", "h"); // no site
        let hosts = vec![a, b, c, d];
        // ranked order is [0,1,2,3]; group_order keeps that order within each section.
        let order = group_order(&hosts, &[0, 1, 2, 3]);
        let names: Vec<&str> = order.iter().map(|&i| hosts[i].name.as_str()).collect();
        // alpha section (b, c), then zeta (a), then the (no site) group (d).
        assert_eq!(names, vec!["b", "c", "a", "d"]);
    }

    #[test]
    fn sites_manager_rename_cascades_and_delete_orphans() {
        let mut app = test_app();
        app.sites = vec![Site::new("dc1"), Site::new("dc2")];
        app.hosts[0].site = Some("dc1".into());
        app.hosts[1].site = Some("dc2".into());

        app.on_key(key(KeyCode::F(3))); // open the sites manager
        assert!(app.sites_manager.is_some());
        app.on_key(key(KeyCode::Enter)); // edit dc1
        for _ in 0..3 {
            app.on_key(key(KeyCode::Backspace));
        }
        for c in "prod".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(ctrl(KeyCode::Char('s'))); // commit form: rename dc1 -> prod
        app.on_key(key(KeyCode::Down)); // select dc2
        app.on_key(key(KeyCode::Char('d'))); // confirm-delete prompt
        app.on_key(key(KeyCode::Char('y'))); // delete dc2
        app.on_key(ctrl(KeyCode::Char('s'))); // save the manager

        assert!(app.sites_manager.is_none());
        assert_eq!(app.sites.len(), 1);
        assert_eq!(app.sites[0].name, "prod");
        assert_eq!(app.hosts[0].site.as_deref(), Some("prod")); // rename cascaded
        assert_eq!(app.hosts[1].site, None); // dc2 deleted -> host orphaned
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
            sites: Vec::new(),
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
