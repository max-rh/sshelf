//! Application state, key handling, and the synchronous event loop.
//!
//! `on_key` is pure (mutates `App`, returns an `Outcome`) so it can be unit-tested by
//! feeding synthetic key events. The event loop is a thin wrapper around it.
//!
//! Connecting is deferred until *after* the terminal is restored: `on_key` returns
//! `Outcome::Connect`, the loop records it and quits, and `run` performs the `exec()` handoff
//! once the TUI is torn down (so ssh inherits a clean TTY).
//!
//! The one exception is tmux mode (`tmux = "window" | "pane"`, and only when sshelf itself runs
//! inside tmux): the connection opens in a new window/pane and sshelf **keeps running**, so
//! several hosts can be fired off in a row. Connections that can't cross the tmux boundary
//! without putting a secret in argv fall back to the in-place handoff — see `ssh::tmux_fallback`
//! and D-025.

use std::io::{IsTerminal, Stderr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use zeroize::Zeroizing;

use crate::config::{Config, Tmux};
use crate::first_connect::{self, Decision};
use crate::forwards::{self, ForwardsState};
use crate::import;
use crate::model::{Credential, Host, HostsFile, Site};
use crate::paths::Paths;
use crate::search;
use crate::secrets;
use crate::ssh;
use crate::state::FrecencyState;
use crate::store;
use crate::transfer::{self, TransferOutcome};
use crate::ui;
use crate::ui::bulk_secret::{BulkOutcome, BulkRequest, BulkSecretPopup, Mode as BulkMode};
use crate::ui::forward_popup::{ForwardPopup, ForwardPopupOutcome};
use crate::ui::forwards::{ForwardsManager, ForwardsOutcome};
use crate::ui::settings::{Settings, SettingsOutcome};
use crate::ui::sites::{SitesManager, SitesOutcome};
use crate::ui::two_factor::{TwoFactorOutcome, TwoFactorPopup};
use crate::ui::wizard::{Wizard, WizardOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    List,
    Help,
}

/// Shown when an action needs a host but the cursor isn't on one — an empty database, or a
/// filter that matches nothing.
const NO_HOST_SELECTED: &str = "no host under the cursor — clear the filter (esc), or add one (^a)";

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
    /// Open the new-port-forward popup for this host (the loop opens it after key handling, so
    /// resolving the host stays out of the testable `on_key`).
    OpenForwardPopup(usize),
}

pub struct App {
    pub hosts: Vec<Host>,
    /// Defined sites (groups + optional inherited SSH defaults).
    pub sites: Vec<Site>,
    /// Defined shared credentials (one secret, many hosts). Held so every save round-trips
    /// them: `persist_hosts` rebuilds the whole file from these fields.
    pub credentials: Vec<Credential>,
    /// Hosts marked for a bulk action, by **id** rather than index, so filtering and
    /// reordering the list cannot scramble the selection.
    pub marked: Vec<String>,
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
    /// The new-port-forward popup (Ctrl-f), when open.
    pub forward_popup: Option<ForwardPopup>,
    /// The port-forwards manager (F4), when open.
    pub forwards_manager: Option<ForwardsManager>,
    /// Active background port-forwards (the `forwards.json` ledger).
    pub forwards_state: ForwardsState,
    /// The 2FA verification-code popup, shown before connecting to a `requires_2fa` host.
    pub two_factor: Option<TwoFactorPopup>,
    /// The bulk-credentials popup (^k), when open.
    pub bulk_secret: Option<BulkSecretPopup>,
    /// A one-time 2FA code collected for the pending connect; passed to `ssh` after teardown.
    /// Held zeroizing: it is a live secret from the moment it is typed until the handoff.
    pub pending_2fa_code: Option<zeroize::Zeroizing<String>>,
    /// The dual-pane file-transfer screen, when open.
    pub transfer: Option<transfer::TransferScreen>,
    /// Transient status line (cleared on next keypress).
    pub status: Option<String>,
    pub should_quit: bool,
    /// Set when the user chose a host; the real connect happens after terminal restore.
    pub pending_connect: Option<usize>,
    /// A line to print just before the in-place handoff — currently why tmux mode stepped
    /// aside for this connection, so the missing window isn't a mystery. Printed after the TUI
    /// is down (nothing else is visible from inside the alternate screen).
    pub connect_note: Option<String>,
    /// Whether sshelf itself is running inside tmux. Read once: `$TMUX` is set by the server
    /// for the pane we were launched in and cannot change while this process lives.
    pub in_tmux: bool,
    /// What `sshelf add --from-ssh` dropped or rewrote, joined for the status line. Shown once
    /// the form it opened closes, whichever way it closes.
    pub add_notes: Option<String>,
    /// Pick mode (`sshelf pick`): `Enter` records the row and quits **without** connecting, so
    /// the name can be printed for a shell to use. Nothing else about the list changes.
    pub pick: bool,
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
            credentials: Vec::new(),
            marked: Vec::new(),
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
            forward_popup: None,
            forwards_manager: None,
            forwards_state: ForwardsState::default(),
            two_factor: None,
            bulk_secret: None,
            pending_2fa_code: None,
            transfer: None,
            status: None,
            should_quit: false,
            pending_connect: None,
            connect_note: None,
            in_tmux: ssh::inside_tmux(),
            add_notes: None,
            pick: false,
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
            &self.sites,
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

    /// The message for a failed write of the host database. Every caller has the same next
    /// action — the file is named so it can be checked — so they share one wording.
    fn save_failed(&self, e: &anyhow::Error) -> String {
        format!(
            "hosts NOT saved to {}: {e:#} — nothing was written",
            self.hosts_path.display()
        )
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
        let file = HostsFile::with_parts(
            self.credentials.clone(),
            self.sites.clone(),
            self.hosts.clone(),
        );
        store::save_hosts(&self.hosts_path, &file)?;
        // Best-effort: the ssh_config export is derived data — a refresh failure must never
        // block saving hosts (the next `sshelf export` run reports it).
        let _ = crate::export::refresh_if_exported(&self.paths, &file);
        Ok(())
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
        if self.forward_popup.is_some() {
            self.on_key_forward_popup(key);
            return Outcome::Continue;
        }
        if self.forwards_manager.is_some() {
            self.on_key_forwards(key);
            return Outcome::Continue;
        }
        if self.two_factor.is_some() {
            self.on_key_two_factor(key);
            return Outcome::Continue;
        }
        if self.bulk_secret.is_some() {
            self.on_key_bulk_secret(key);
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
        if self.pick && !pick_allows(key.code, ctrl) {
            return Outcome::Continue;
        }
        self.status = None;
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) => return Outcome::Quit,
            (KeyCode::Esc, _) => {
                // Marks first, then the query, then quit: esc always undoes the most recent
                // thing, and a stray quit can never throw away a selection.
                if !self.marked.is_empty() {
                    let n = self.marked.len();
                    self.marked.clear();
                    self.set_status(format!("cleared {n} mark(s)"));
                    return Outcome::Continue;
                }
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
            (KeyCode::Char('f'), true) => match self.current() {
                Some(i) => return Outcome::OpenForwardPopup(i),
                None => self.set_status(NO_HOST_SELECTED),
            },
            (KeyCode::Char('a'), true) => {
                let names = self.site_names();
                self.wizard = Some(Wizard::new_add(&names));
            }
            (KeyCode::Char('e'), true) => match self.current() {
                Some(i) => {
                    let names = self.site_names();
                    self.wizard = Some(Wizard::from_host(&self.hosts[i], &names));
                }
                None => self.set_status(NO_HOST_SELECTED),
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
            (KeyCode::Char('s'), true) => match self.current() {
                Some(i) => {
                    let id = self.hosts[i].id.clone();
                    match self.marked.iter().position(|m| *m == id) {
                        Some(at) => {
                            self.marked.remove(at);
                        }
                        None => self.marked.push(id),
                    }
                    let n = self.marked.len();
                    // What a mark is *for* differs by mode: a bulk credential write in the TUI,
                    // the set of names to print in pick mode, where ^k is not even reachable.
                    let next = if self.pick {
                        "↵ prints every marked host"
                    } else {
                        "^k set credentials"
                    };
                    self.set_status(match n {
                        0 => "nothing marked".to_string(),
                        1 => format!("1 host marked · {next} · esc clears"),
                        n => format!("{n} hosts marked · {next} · esc clears"),
                    });
                }
                None => self.set_status(NO_HOST_SELECTED),
            },
            (KeyCode::Char('k'), true) => self.open_bulk_secret(),
            (KeyCode::Char('o'), true) => self.import_ssh_config(),
            (KeyCode::Down, false) | (KeyCode::Char('n'), true) => self.move_down(),
            (KeyCode::Up, false) | (KeyCode::Char('p'), true) => self.move_up(),
            (KeyCode::F(1), _) => self.screen = Screen::Help,
            (KeyCode::F(2), _) => self.open_settings(),
            (KeyCode::F(3), _) => self.open_sites(),
            (KeyCode::F(4), _) => self.open_forwards(),
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
                let name = self
                    .hosts
                    .iter()
                    .find(|h| h.id == id)
                    .map(|h| h.name.clone());
                match self.persist_hosts() {
                    Ok(()) => match secret_err {
                        Some(e) => self.set_status(format!(
                            "host saved, but its secret was not stored: {e} — retry with `sshelf set-password {}`",
                            name.unwrap_or_else(|| id.clone())
                        )),
                        None => self.set_status(if updated {
                            "host updated"
                        } else {
                            "host added"
                        }),
                    },
                    Err(e) => self.set_status(self.save_failed(&e)),
                }
                self.recompute();
            }
        }
        if self.wizard.is_none()
            && let Some(notes) = self.add_notes.take()
        {
            self.status = Some(match self.status.take() {
                Some(status) => format!("{status} · {notes}"),
                None => notes,
            });
        }
    }

    fn open_settings(&mut self) {
        self.settings = Some(Settings::new(
            self.paths.config_file().display().to_string(),
            self.config.hosts_file.clone(),
            self.paths.default_hosts_display(),
            self.config.tmux,
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
                    Err(e) => self.set_status(self.save_failed(&e)),
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
            SettingsOutcome::Save { hosts_file, tmux } => {
                self.settings = None;
                // Resolve the proposed path WITHOUT committing config yet.
                let proposed = Config {
                    hosts_file: hosts_file.clone(),
                    ..self.config.clone()
                };
                let new_path = proposed.hosts_path(&self.paths);
                // The tmux mode has no side effects, so it commits regardless of the path step.
                self.config.tmux = tmux;

                if new_path == self.hosts_path {
                    self.config.hosts_file = hosts_file;
                    match self.config.save(&self.paths.config_file()) {
                        Ok(()) => self.set_status("settings saved"),
                        Err(e) => self.set_status(format!(
                            "settings not saved to {}: {e} — check the file is writable",
                            self.paths.config_file().display()
                        )),
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
                        Err(e) => Err(format!(
                            "could not read {}: {e} — fix it or pick another path",
                            new_path.display()
                        )),
                    }
                } else {
                    let file = HostsFile::with_parts(
                        self.credentials.clone(),
                        self.sites.clone(),
                        self.hosts.clone(),
                    );
                    match store::save_hosts(&new_path, &file) {
                        Ok(()) => Ok(format!("hosts moved to {}", new_path.display())),
                        Err(e) => Err(format!(
                            "could not write {}: {e} — the hosts file is unchanged",
                            new_path.display()
                        )),
                    }
                };

                match outcome {
                    Ok(msg) => {
                        // Commit config only now that the hosts step succeeded.
                        self.config.hosts_file = hosts_file;
                        self.hosts_path = new_path;
                        self.recompute();
                        // Adopting an existing file swapped the host set — keep the ssh_config
                        // export (if enabled) in step with it. Best-effort, like persist_hosts.
                        let file = HostsFile::with_parts(
                            self.credentials.clone(),
                            self.sites.clone(),
                            self.hosts.clone(),
                        );
                        let _ = crate::export::refresh_if_exported(&self.paths, &file);
                        match self.config.save(&self.paths.config_file()) {
                            Ok(()) => self.set_status(format!("settings saved · {msg}")),
                            Err(e) => self.set_status(format!(
                                "hosts updated, but {} was not saved: {e} — the new location won't stick",
                                self.paths.config_file().display()
                            )),
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
            let own_secret = own_secret_key(&self.hosts, &c.id);
            self.hosts.retain(|h| h.id != c.id);
            self.state.stats.remove(&c.id);
            let _ = self.state.save(&self.paths.state_file());
            if let Some(key) = own_secret {
                let _ = secrets::delete_password(&self.paths.vault_file(), &key);
            }
            match self.persist_hosts() {
                Ok(()) => self.set_status(format!("deleted {}", c.name)),
                Err(e) => self.set_status(self.save_failed(&e)),
            }
            self.recompute();
        }
    }

    /// Import new hosts from ~/.ssh/config (read-only), skipping names we already have.
    fn import_ssh_config(&mut self) {
        let path = match import::default_config_path() {
            Some(p) if p.exists() => p,
            Some(p) => {
                self.set_status(format!(
                    "no ssh config at {} — nothing to import; add a host with ^a instead",
                    p.display()
                ));
                return;
            }
            None => {
                self.set_status(
                    "$HOME is not set — sshelf can't locate ~/.ssh/config to import from",
                );
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
                    Err(e) => self.set_status(format!(
                        "{added} host(s) parsed, but {} could not be written: {e}",
                        self.hosts_path.display()
                    )),
                }
            }
            Err(e) => self.set_status(format!("could not import {}: {e}", path.display())),
        }
    }

    /// Spawn the transfer worker for `idx` and open the dual-pane screen. Mirrors connect: the
    /// event loop calls this so the side effects (a worker thread, a secrets lookup) stay out
    /// of the testable `on_key`.
    fn open_transfer(&mut self, idx: usize) {
        // Resolve the host's site defaults so transfers ride the site's bastion / use its
        // default user; `id` is preserved so the secrets lookup below is unaffected.
        let host = self.hosts[idx].with_site_defaults(&self.sites);
        let has_secret = self.has_secret(&host.secret_key());
        let start = std::env::current_dir()
            .ok()
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"));
        match transfer::TransferScreen::open(&host, has_secret, start) {
            Ok(screen) => self.transfer = Some(screen),
            Err(e) => self.set_status(format!(
                "could not open the transfer screen for {}: {e}",
                host.name
            )),
        }
    }

    /// True when a login password / key passphrase is stored for this host (so the askpass
    /// helper has something to supply). Mirrors what connect and the transfer worker ask.
    fn has_secret(&self, key: &str) -> bool {
        secrets::get_password(&self.paths.vault_file(), key)
            .ok()
            .flatten()
            .is_some()
    }

    /// True when connecting to `idx` could start with the first-connect question (D-035). A 2FA
    /// host then skips its code popup, so the secret and the code are both asked on the terminal
    /// after teardown, in that order.
    fn asks_first_secret(&self, idx: usize) -> bool {
        let host = self.hosts[idx].with_site_defaults(&self.sites);
        first_connect::assess(&host, || self.has_secret(&host.secret_key()), false).0
            != Decision::Connect
    }

    /// Persist this host's usage. Returns a warning to surface if the save failed: usage must
    /// reach disk *before* any handoff, since neither `exec()` nor a tmux window comes back.
    fn record_use(&mut self, id: &str) -> Option<String> {
        self.state.record_use(id);
        self.state
            .save(&self.paths.state_file())
            .err()
            .map(|e| format!("usage not saved: {e}"))
    }

    /// Connect to the host at `idx`.
    ///
    /// In tmux mode this opens a new window/pane and **returns to the picker**; otherwise (and
    /// whenever the connection would have to carry a secret across tmux's argv) it queues the
    /// in-place `exec()` handoff the event loop performs once the terminal is restored.
    fn connect(&mut self, idx: usize) {
        let mode = self.config.tmux;
        if mode == Tmux::Off || !self.in_tmux {
            return self.queue_exec_connect(idx);
        }
        // Site defaults (bastion/user/port/identity) apply exactly as on an in-place connect;
        // `id` is preserved, so the secrets lookup and frecency still key correctly.
        let host = self.hosts[idx].with_site_defaults(&self.sites);
        // A queued 2FA code settles this on its own (it could only cross into tmux through
        // argv), so don't ask the keyring anything we won't use.
        let has_code = self.pending_2fa_code.is_some();
        let wire_askpass = !has_code && self.has_secret(&host.secret_key());
        // With nothing stored, the first connect may ask for the secret, and that question has
        // to be asked on this terminal (D-035). The probe waits for the in-place path; only the
        // part that needs no network is decided here.
        let first_secret = !has_code
            && !wire_askpass
            && first_connect::assess(&host, || false, true).0 == Decision::StepAside;
        if let Err(reason) = ssh::tmux_fallback(&host, wire_askpass, has_code, first_secret) {
            self.connect_note = Some(reason.message().to_string());
            return self.queue_exec_connect(idx);
        }
        // Persist usage BEFORE the spawn — the new window owns the connection from here on.
        let warning = self.record_use(&host.id);
        match ssh::tmux_connect(mode, &host, wire_askpass) {
            Ok(name) => {
                let mut msg = format!("opened in tmux {}: {name}", mode.noun());
                if let Some(w) = warning {
                    msg.push_str(&format!(" · {w}"));
                }
                self.set_status(msg);
            }
            Err(e) => self.set_status(e),
        }
    }

    /// Hand the connection to the event loop: quit, restore the terminal, then `exec()` ssh.
    fn queue_exec_connect(&mut self, idx: usize) {
        self.pending_connect = Some(idx);
        self.should_quit = true;
    }

    /// The names `sshelf pick` prints: every marked host, or the row that was chosen when
    /// nothing is marked. Empty when the user quit without choosing anything.
    ///
    /// Marks win over the cursor, and they are read from `hosts` rather than the display order,
    /// so a filter typed after marking can't silently drop one — the same rule the bulk
    /// credentials popup follows.
    pub fn picked_names(&self) -> Vec<String> {
        let Some(idx) = self.pending_connect else {
            return Vec::new();
        };
        if self.marked.is_empty() {
            return vec![self.hosts[idx].name.clone()];
        }
        self.hosts
            .iter()
            .filter(|h| self.is_marked(&h.id))
            .map(|h| h.name.clone())
            .collect()
    }

    /// Open the 2FA code popup for `idx` (a `requires_2fa` host). On submit it queues the code +
    /// the pending connect and quits, so the real `exec()` happens after terminal restore.
    fn open_two_factor(&mut self, idx: usize) {
        self.two_factor = Some(TwoFactorPopup::new(idx, self.hosts[idx].name.clone()));
    }

    fn on_key_two_factor(&mut self, key: KeyEvent) {
        let outcome = match self.two_factor.as_mut() {
            Some(p) => p.handle_key(key),
            None => return,
        };
        match outcome {
            TwoFactorOutcome::Continue => {}
            TwoFactorOutcome::Cancel => self.two_factor = None,
            TwoFactorOutcome::Submit(code) => {
                let idx = self.two_factor.as_ref().map(TwoFactorPopup::host_idx);
                self.two_factor = None;
                if let Some(idx) = idx {
                    // Queue the code first: `connect` sees it and (in tmux mode) steps aside,
                    // because a code can only cross into tmux through argv.
                    self.pending_2fa_code = Some(code);
                    self.connect(idx);
                }
            }
        }
    }

    /// True when `id` is marked for a bulk action.
    pub fn is_marked(&self, id: &str) -> bool {
        self.marked.iter().any(|m| m == id)
    }

    /// The hosts a bulk action applies to: whatever is marked, or everything the filter shows
    /// when nothing is. Marks win so a filter typed afterwards cannot silently widen the set.
    fn bulk_targets(&self) -> (Vec<usize>, bool) {
        if self.marked.is_empty() {
            return (self.order.clone(), false);
        }
        let idxs = self
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, h)| self.is_marked(&h.id))
            .map(|(i, _)| i)
            .collect();
        (idxs, true)
    }

    fn open_bulk_secret(&mut self) {
        let (targets, from_marks) = self.bulk_targets();
        if targets.is_empty() {
            self.set_status("no hosts to set a credential for".to_string());
            return;
        }
        let with_secret = targets
            .iter()
            .filter(|&&i| self.has_secret(&self.hosts[i].secret_key()))
            .count();
        self.bulk_secret = Some(BulkSecretPopup::new(targets, with_secret, from_marks));
    }

    fn on_key_bulk_secret(&mut self, key: KeyEvent) {
        let outcome = match self.bulk_secret.as_mut() {
            Some(p) => p.handle_key(key),
            None => return,
        };
        match outcome {
            BulkOutcome::Continue => {}
            BulkOutcome::Cancel => self.bulk_secret = None,
            BulkOutcome::Apply(req) => {
                let targets = self
                    .bulk_secret
                    .as_ref()
                    .map(|p| p.targets().to_vec())
                    .unwrap_or_default();
                self.bulk_secret = None;
                let status = self.apply_bulk_secret(&req, &targets);
                drop(req);
                self.marked.clear();
                self.recompute();
                self.set_status(status);
            }
        }
    }

    /// Apply a bulk credential change and return the status line for it. Writes secrets first
    /// and `hosts.toml` once at the end, so a failed save never leaves hosts pointing at a
    /// credential whose secret was not stored.
    fn apply_bulk_secret(&mut self, req: &BulkRequest, targets: &[usize]) -> String {
        let vault = self.paths.vault_file();
        let mut stored = 0usize;
        let mut skipped = 0usize;
        let mut failed: Vec<String> = Vec::new();

        match req.mode {
            BulkMode::Each => {
                let Some(secret) = req.secret.as_deref() else {
                    return "nothing typed, nothing stored".to_string();
                };
                for &i in targets {
                    let key = self.hosts[i].secret_key();
                    if !req.replace_existing && self.has_secret(&key) {
                        skipped += 1;
                        continue;
                    }
                    match secrets::store_password(&vault, &key, secret) {
                        Ok(()) => stored += 1,
                        Err(_) => failed.push(self.hosts[i].name.clone()),
                    }
                }
            }
            BulkMode::Shared => {
                let Some(name) = req.credential.as_deref() else {
                    return "no credential name".to_string();
                };
                let key = crate::model::credential_secret_key(name);
                // One write for the whole set, which is the point of a shared credential.
                if let Some(secret) = req.secret.as_deref() {
                    if req.replace_existing || !self.has_secret(&key) {
                        if let Err(e) = secrets::store_password(&vault, &key, secret) {
                            return format!("could not store the credential: {e:#}");
                        }
                        stored = 1;
                    } else {
                        skipped = 1;
                    }
                }
                for &i in targets {
                    self.hosts[i].credential = Some(name.to_string());
                }
                if crate::model::find_credential(&self.credentials, name).is_none() {
                    self.credentials
                        .push(crate::model::Credential::new(name.to_string()));
                }
                if let Err(e) = self.persist_hosts() {
                    return self.save_failed(&e);
                }
                let secret_note = match (stored, skipped) {
                    (1, _) => " and set its password",
                    (_, 1) => " (its stored password was kept)",
                    _ => " (no password stored for it yet)",
                };
                return format!(
                    "{} host(s) now use '{}'{}",
                    targets.len(),
                    crate::display::sanitize(name),
                    secret_note
                );
            }
            BulkMode::Unbind => {
                let mut changed = 0usize;
                for &i in targets {
                    if self.hosts[i].credential.take().is_some() {
                        changed += 1;
                    }
                }
                if let Err(e) = self.persist_hosts() {
                    return self.save_failed(&e);
                }
                return match changed {
                    0 => "none of those hosts used a shared credential".to_string(),
                    n => format!("{n} host(s) unbound · each needs its own password now"),
                };
            }
        }

        let mut parts = vec![format!("stored a password for {stored} host(s)")];
        if skipped > 0 {
            parts.push(format!("{skipped} kept the one they had (^r replaces)"));
        }
        if !failed.is_empty() {
            parts.push(format!(
                "failed for {}",
                crate::display::sanitize(&failed.join(", "))
            ));
        }
        parts.join(" · ")
    }

    /// Open the new-port-forward popup for `idx`. Called from the loop (mirrors `open_transfer`).
    fn open_forward_popup(&mut self, idx: usize) {
        self.forward_popup = Some(ForwardPopup::new(idx, self.hosts[idx].name.clone()));
    }

    fn on_key_forward_popup(&mut self, key: KeyEvent) {
        let outcome = match self.forward_popup.as_mut() {
            Some(p) => p.handle_key(key),
            None => return,
        };
        match outcome {
            ForwardPopupOutcome::Continue => {}
            ForwardPopupOutcome::Cancel => self.forward_popup = None,
            ForwardPopupOutcome::Create { kind, spec } => {
                let Some(idx) = self.forward_popup.as_ref().map(ForwardPopup::host_idx) else {
                    return;
                };
                // Resolve site defaults (bastion/user/port/identity) before building the ssh
                // command; secrets/frecency still key on the original `host.id`.
                let host = self.hosts[idx].with_site_defaults(&self.sites);
                let has_secret = self.has_secret(&host.secret_key());
                // Spawning blocks briefly (~2.5s) to catch a bind/auth failure; see forwards.rs.
                match forwards::spawn_forward(&host, &self.hosts[idx].name, has_secret, kind, spec)
                {
                    Ok(entry) => {
                        let display = entry.display.clone();
                        self.forwards_state.forwards.push(entry);
                        if let Err(e) = self.forwards_state.save(&self.paths.forwards_file()) {
                            self.set_status(format!(
                                "forward up, but not recorded: {e} — F4 will lose it when sshelf restarts"
                            ));
                        } else {
                            self.set_status(format!("forward up · {display}"));
                        }
                        self.forward_popup = None;
                    }
                    Err(e) => {
                        if let Some(p) = self.forward_popup.as_mut() {
                            p.set_error(e);
                        }
                    }
                }
            }
        }
    }

    /// Open the port-forwards manager (F4). Reconciles first so it opens showing only forwards
    /// that are still actually running.
    fn open_forwards(&mut self) {
        self.reconcile_forwards();
        self.forwards_manager = Some(ForwardsManager::new(self.forwards_state.forwards.clone()));
    }

    fn on_key_forwards(&mut self, key: KeyEvent) {
        let outcome = match self.forwards_manager.as_mut() {
            Some(m) => m.handle_key(key),
            None => return,
        };
        match outcome {
            ForwardsOutcome::Continue => {}
            ForwardsOutcome::Close => self.forwards_manager = None,
            ForwardsOutcome::Kill(id) => {
                if let Some(entry) = self.forwards_state.forwards.iter().find(|e| e.id == id) {
                    forwards::kill(entry);
                }
                self.forwards_state.forwards.retain(|e| e.id != id);
                let _ = self.forwards_state.save(&self.paths.forwards_file());
                self.refresh_forwards_manager();
                self.set_status("forward stopped");
            }
        }
    }

    /// Drop ledger entries whose process has ended; persist if anything changed.
    fn reconcile_forwards(&mut self) {
        if !forwards::reconcile(&mut self.forwards_state).is_empty() {
            let _ = self.forwards_state.save(&self.paths.forwards_file());
        }
    }

    /// While the manager is open, reconcile and refresh its snapshot (called each tick, so a
    /// forward that ends — externally or on its own — disappears within ~100ms without a keypress).
    pub fn refresh_forwards_manager(&mut self) {
        if self.forwards_manager.is_none() {
            return;
        }
        self.reconcile_forwards();
        if let Some(m) = self.forwards_manager.as_mut() {
            m.set_forwards(self.forwards_state.forwards.clone());
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

/// How pick mode draws itself.
///
/// Inline is the default and the point of the mode: the list appears in the rows under your
/// prompt, the way `Ctrl-r` or atuin does, and the prompt is still there when it closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickView {
    /// `height` rows anchored at the cursor, with no alternate screen.
    Inline(u16),
    /// The whole terminal, on the alternate screen (`--full-screen`).
    FullScreen,
}

/// Resolve how pick mode draws: the flag wins, then the `pick_height` config key.
///
/// A height of `0` from either means the whole screen, which is how atuin spells the same
/// choice. Anything taller than the terminal is clamped to leave a row for the prompt the
/// picker opened under, so a careless value cannot scroll it out of sight. `rows` of 0 means the
/// size could not be read, and then the request is taken as given.
fn pick_view_for(
    configured: u16,
    requested: Option<u16>,
    full_screen: bool,
    rows: u16,
) -> PickView {
    if full_screen {
        return PickView::FullScreen;
    }
    let height = requested.unwrap_or(configured);
    if height == 0 {
        return PickView::FullScreen;
    }
    let limit = rows.saturating_sub(1);
    let height = if rows == 0 { height } else { height.min(limit) };
    // Below a search box, one row of list and the hint there is nothing to look at, and a
    // one-row terminal is not worth a special case.
    PickView::Inline(height.max(5))
}

/// What the TUI opens on.
// `Prefilled` carries a Host (large) while the others are unit variants; this is built once
// and consumed at startup, so the size difference doesn't matter.
#[allow(clippy::large_enum_variant)]
pub enum Start {
    /// The host list (`sshelf`).
    List,
    /// The host list in pick mode (`sshelf pick`), with the search box prefilled.
    Pick {
        query: String,
        height: Option<u16>,
        full_screen: bool,
    },
    /// The empty add form (`sshelf add`).
    Add,
    /// The add form filled in by `sshelf add --from-ssh`, with the parser's notes and a secret
    /// read by `--password-stdin`, if any.
    Prefilled {
        host: Host,
        notes: Vec<String>,
        secret: Option<Zeroizing<String>>,
    },
}

/// Set up the terminal, run the loop, restore the terminal, then (if a host was chosen)
/// perform the `exec()` handoff into ssh.
pub fn run() -> Result<()> {
    run_with(Start::List)
}

/// Like [`run`], but with the add-host form already open (`sshelf add`).
pub fn run_add() -> Result<()> {
    run_with(Start::Add)
}

/// `sshelf pick`: open the list, print the chosen host's name on stdout, connect to nothing.
///
/// `height` is the `--height` flag and falls back to the `pick_height` config key, which
/// `run_with` resolves once it has loaded the config.
pub fn run_pick(query: &str, height: Option<u16>, full_screen: bool) -> Result<()> {
    run_with(Start::Pick {
        query: query.to_string(),
        height,
        full_screen,
    })
}

/// Like [`run_add`], with the form filled in from a parsed ssh command line. Nothing is saved
/// until the form is.
pub fn run_add_prefilled(
    host: Host,
    notes: Vec<String>,
    secret: Option<Zeroizing<String>>,
) -> Result<()> {
    run_with(Start::Prefilled {
        host,
        notes,
        secret,
    })
}

fn run_with(start: Start) -> Result<()> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let _ = Config::ensure_default_file(&paths.config_file()); // best-effort
    let config = Config::load(&paths.config_file())?;
    let file = store::load_hosts(&config.hosts_path(&paths))?;
    let state = FrecencyState::load(&paths.state_file())?;
    let mut app = App::new(file.hosts, file.sites, state, config, paths);
    app.credentials = file.credentials;
    // Load the active-forward ledger and reconcile it against reality (drop any that ended while
    // sshelf was closed), so the user sees only forwards that are still actually running.
    match ForwardsState::load(&app.paths.forwards_file()) {
        Ok(fs) => {
            app.forwards_state = fs;
            app.reconcile_forwards();
        }
        Err(e) => eprintln!("sshelf: warning: could not load forwards: {e:#}"),
    }
    let mut pick_view = None;
    match start {
        Start::List => {}
        Start::Pick {
            query,
            height,
            full_screen,
        } => {
            app.pick = true;
            app.query = query;
            app.recompute();
            let rows = ratatui::crossterm::terminal::size()
                .map(|(_, rows)| rows)
                .unwrap_or(0);
            pick_view = Some(pick_view_for(
                app.config.pick_height,
                height,
                full_screen,
                rows,
            ));
        }
        Start::Add => {
            let names = app.site_names();
            app.wizard = Some(Wizard::new_add(&names));
        }
        Start::Prefilled {
            host,
            notes,
            secret,
        } => {
            let names = app.site_names();
            let mut wizard = Wizard::prefill(&host, &names);
            if let Some(secret) = &secret {
                wizard.set_secret(secret);
            }
            app.wizard = Some(wizard);
            app.add_notes = (!notes.is_empty()).then(|| notes.join(" · "));
        }
    }

    if let Some(view) = pick_view {
        return finish_pick(&mut app, view);
    }

    let mut terminal = ratatui::init();
    let loop_result = event_loop(&mut terminal, &mut app);
    ratatui::restore();
    loop_result?;

    if let Some(idx) = app.pending_connect {
        // Now that the alternate screen is gone, explain (once) why tmux mode stepped aside.
        if let Some(note) = &app.connect_note {
            eprintln!("sshelf: {note}");
        }
        // Resolve the host's site defaults (bastion/user/port/identity) for the real connect.
        let host = app.hosts[idx].with_site_defaults(&app.sites);
        // Persist usage BEFORE exec() — nothing runs after a successful exec.
        app.state.record_use(&host.id);
        if let Err(e) = app.state.save(&app.paths.state_file()) {
            eprintln!("sshelf: warning: could not save state: {e:#}");
        }
        // The same tail `sshelf <host>` runs: the first-connect capture, the 2FA code (the one
        // from the popup, if the user entered it there), then the exec. Returns only on failure.
        return Err(crate::handoff(
            &host,
            &app.paths,
            app.pending_2fa_code.take(),
            &crate::ssh::RemoteCommand::default(),
        ));
    }
    Ok(())
}

/// Pick mode's tail: run the loop on a stderr terminal, then print what was chosen.
///
/// Nothing connects and nothing is recorded in the frecency state. A name on stdout is the whole
/// product, and the connect that usually follows records its own use, so picking a host twice to
/// run two commands on it doesn't count as using it twice.
fn finish_pick(app: &mut App, view: PickView) -> Result<()> {
    let (mut terminal, stdout_swap) = init_pick_terminal(view)?;
    let loop_result = event_loop(&mut terminal, app);
    // Inline mode draws in the normal buffer, so the rows it used have to be wiped or they stay
    // on screen above the next prompt. Moving to the viewport's own origin first makes `clear`
    // (which restores wherever the cursor happened to be) leave it exactly where the picker
    // started, which is where the shell will redraw its prompt.
    if matches!(view, PickView::Inline(_)) {
        let origin = terminal.get_frame().area().as_position();
        let _ = terminal.set_cursor_position(origin);
        let _ = terminal.clear();
    }
    restore_pick_terminal(view);
    // Only now: `clear` asks the terminal where the cursor is, and that question goes out on
    // fd 1, which the swap is still holding. Dropping it hands the pipe back for the name.
    drop(stdout_swap);
    loop_result?;

    let names = app.picked_names();
    if names.is_empty() {
        // Quit without choosing (esc / ^c): no output at all, and a non-zero status so a shell
        // widget can tell "the user backed out" from "the user chose a host".
        std::process::exit(1);
    }
    for name in names {
        println!("{name}");
    }
    Ok(())
}

/// Pick mode's terminal, on **stderr**.
///
/// ratatui's `init`/`restore` are stdout-only, and in pick mode stdout carries the chosen name:
/// `LeaveAlternateScreen` written there would land in the `$(sshelf pick)` the shell captures,
/// so the pair is spelled out here against stderr. The panic hook is replaced for the same
/// reason — ratatui's own hook restores the wrong stream.
///
/// Inline mode stays out of the alternate screen entirely. ratatui anchors the viewport at the
/// cursor row and scrolls the terminal if the prompt sits too near the bottom to fit. To place
/// it, it has to know where the cursor is, and crossterm asks the terminal that with `ESC [ 6 n`
/// written to `io::stdout()` rather than to the backend's own writer. In pick mode stdout is the
/// pipe carrying the chosen name, so the question would be captured instead of answered and the
/// setup would fail two seconds later with "the cursor position could not be read". So fd 1 is
/// lent to the terminal for as long as the list is up, and the returned guard hands the pipe
/// back. Resize does the same query mid-loop, which is the other reason the guard lives that
/// long.
fn init_pick_terminal(
    view: PickView,
) -> Result<(
    ratatui::Terminal<CrosstermBackend<Stderr>>,
    Option<StdoutSwap>,
)> {
    use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
    use ratatui::{TerminalOptions, Viewport};

    if !std::io::stderr().is_terminal() {
        anyhow::bail!(
            "`sshelf pick` draws the list on stderr and needs a terminal there; stdout is \
             free for the chosen name, so use it as `host=$(sshelf pick)`"
        );
    }
    // Pick mode is normally launched from a shell widget, where none of the three streams is the
    // terminal (zle keeps it) and crossterm's own fallback to the `/dev/tty` alias stops at
    // "Failed to initialize input reader" on macOS. Point fd 0 at the real device, the same way
    // the add form does for a piped stdin. Nothing else in pick mode reads stdin.
    if !crate::stdin_is_readable_terminal() && !crate::stdin_from_terminal_device() {
        anyhow::bail!("found no terminal to read keys from");
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_pick_terminal(view);
        // A panic cannot clear the inline viewport (that needs the terminal, which is being
        // unwound past), so end the line instead and let the message print below the list
        // rather than over its last row.
        if matches!(view, PickView::Inline(_)) {
            use std::io::Write;
            let _ = std::io::stderr().write_all(b"\r\n");
        }
        previous(info);
    }));
    enable_raw_mode().context("putting the terminal in raw mode")?;
    let backend = CrosstermBackend::new(std::io::stderr());
    match view {
        PickView::FullScreen => {
            execute!(std::io::stderr(), EnterAlternateScreen)
                .context("opening the alternate screen")?;
            let terminal = ratatui::Terminal::new(backend).context("setting up the terminal")?;
            Ok((terminal, None))
        }
        PickView::Inline(height) => {
            let swap = StdoutSwap::to_terminal();
            let mut terminal = ratatui::Terminal::with_options(
                backend,
                TerminalOptions {
                    viewport: Viewport::Inline(height),
                },
            )
            .context("setting up the terminal")?;
            // The viewport lands on the row the prompt is on, and the first draw only writes
            // cells that differ from an all-spaces buffer, so a space never erases what the
            // shell already put there (the prompt showed through the top border). Clearing once
            // up front empties the region and forces that first draw to write every cell.
            terminal.clear().context("clearing the picker's rows")?;
            Ok((terminal, swap))
        }
    }
}

/// fd 1 pointed at the terminal, holding the pipe it displaced aside.
///
/// Dropping it puts the original descriptor back, so a panic unwinding past cannot leave the
/// chosen name writing to the screen. See [`init_pick_terminal`] for what needs this.
struct StdoutSwap(i32);

impl StdoutSwap {
    #[cfg(unix)]
    fn to_terminal() -> Option<Self> {
        use std::os::fd::AsRawFd;

        let device = crate::terminal_device()?;
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&device)
            .ok()?;
        // SAFETY: fd 1 is open, so `dup` returns a second descriptor for whatever it points at
        // (here, the pipe). Both calls are checked, and the terminal descriptor closes on drop
        // once fd 1 holds a copy of it.
        unsafe {
            let saved = libc::dup(libc::STDOUT_FILENO);
            if saved < 0 {
                return None;
            }
            if libc::dup2(tty.as_raw_fd(), libc::STDOUT_FILENO) < 0 {
                libc::close(saved);
                return None;
            }
            Some(StdoutSwap(saved))
        }
    }

    #[cfg(not(unix))]
    fn to_terminal() -> Option<Self> {
        None
    }
}

impl Drop for StdoutSwap {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `dup` and has not been closed; this is the only owner.
        #[cfg(unix)]
        unsafe {
            libc::dup2(self.0, libc::STDOUT_FILENO);
            libc::close(self.0);
        }
    }
}

/// Undo [`init_pick_terminal`]. Called on every exit path, the panic hook included (the fifth
/// hard invariant), and it writes to stderr only.
///
/// Inline mode has no alternate screen to leave: the caller clears the viewport while it still
/// has the terminal to measure it with.
fn restore_pick_terminal(view: PickView) {
    use ratatui::crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};

    let _ = disable_raw_mode();
    if view == PickView::FullScreen {
        let _ = execute!(std::io::stderr(), LeaveAlternateScreen);
    }
}

/// Generic over the backend so the same loop drives the normal stdout terminal and pick mode's
/// stderr one. The bound on `Error` is what lets a draw failure become an `anyhow::Error`; both
/// backends in use here fail with `io::Error`.
fn event_loop<B>(terminal: &mut ratatui::Terminal<B>, app: &mut App) -> Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    while !app.should_quit {
        terminal.draw(|frame| ui::render(frame, app))?;
        // The transfer screen and the forwards manager are "live": poll so worker events /
        // progress animate and a forward that dies disappears, both without needing a keypress.
        if app.transfer.is_some() || app.forwards_manager.is_some() {
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
            {
                dispatch(app, key);
            }
            if let Some(screen) = app.transfer.as_mut() {
                screen.drain_events();
            }
            app.refresh_forwards_manager();
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
            // Pick mode stops here: the row is recorded and the loop ends, nothing is asked and
            // nothing connects. No 2FA code either, since a code would be stale by the time the
            // shell got around to using the name.
            if app.pick {
                app.queue_exec_connect(idx);
            } else if app.hosts[idx].requires_2fa && !app.asks_first_secret(idx) {
                // A connect that may ask for its first secret skips the popup: the secret is
                // asked on the terminal after teardown, and the code after it, since the code
                // goes stale. Otherwise collect the code first and connect on its submit.
                app.open_two_factor(idx);
            } else {
                app.connect(idx);
            }
        }
        Outcome::Yank(idx) => {
            let host = app.hosts[idx].with_site_defaults(&app.sites);
            // Same flag the real connect would use, so a jump host reads the same either way.
            let cmd = ssh::command_string(&host, app.has_secret(&host.secret_key()));
            if ssh::copy_to_clipboard(&cmd) {
                app.set_status(format!("copied: {cmd}"));
            } else {
                app.set_status(cmd);
            }
        }
        Outcome::Transfer(idx) => app.open_transfer(idx),
        Outcome::OpenForwardPopup(idx) => app.open_forward_popup(idx),
        Outcome::Continue => {}
    }
}

/// The keys pick mode answers: filtering, moving, marking, choosing and leaving.
///
/// Everything else is dropped, for two reasons. Pick mode usually runs in an inline viewport of
/// about fifteen rows, and the add form, the settings screen, the two managers and the help
/// overlay are all full-screen designs that cannot be drawn in it. And a chooser that a shell
/// widget fires off has no business editing the database on the way past: `^d` is the one that
/// matters there, since the picker is pointed at whatever inventory the shell was configured
/// with.
fn pick_allows(code: KeyCode, ctrl: bool) -> bool {
    match (code, ctrl) {
        // Quit, mark, and move.
        (KeyCode::Char('c' | 's' | 'n' | 'p'), true) => true,
        // Type to filter, and the keys that undo it or act on it.
        (KeyCode::Char(_) | KeyCode::Backspace, false) => true,
        (KeyCode::Up | KeyCode::Down | KeyCode::Enter | KeyCode::Esc, _) => true,
        _ => false,
    }
}

/// The secret key a host owns outright, or `None` when it reads from a shared credential.
///
/// Deleting a host must only delete a secret of the first kind: the members of a shared
/// credential still answer from it, and one of them leaving is not a reason to lock the rest out.
fn own_secret_key(hosts: &[Host], id: &str) -> Option<String> {
    hosts
        .iter()
        .find(|h| h.id == id)
        .filter(|h| !h.shares_secret())
        .map(|h| h.secret_key())
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

    #[test]
    fn marking_toggles_by_id_and_esc_clears_marks_before_the_query() {
        let mut app = test_app();
        app.on_key(key(KeyCode::Char('p'))); // query: "p" -> both prod hosts
        app.recompute();
        app.on_key(ctrl(KeyCode::Char('s')));
        assert_eq!(app.marked.len(), 1);
        let first = app.marked[0].clone();
        app.move_down();
        app.on_key(ctrl(KeyCode::Char('s')));
        assert_eq!(app.marked.len(), 2);
        assert!(app.is_marked(&first));

        // Toggling the same row again unmarks it.
        app.on_key(ctrl(KeyCode::Char('s')));
        assert_eq!(app.marked.len(), 1);

        // esc clears the marks and keeps the query; a second esc clears the query.
        app.on_key(key(KeyCode::Esc));
        assert!(app.marked.is_empty());
        assert_eq!(app.query, "p");
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.query, "");
    }

    #[test]
    fn a_mark_survives_a_filter_that_hides_it() {
        let mut app = test_app();
        app.on_key(ctrl(KeyCode::Char('s')));
        let marked = app.marked[0].clone();
        // Filter to something that cannot match the marked host.
        app.query = "bastion".into();
        app.recompute();
        assert!(app.is_marked(&marked), "marks are ids, not positions");
        let (targets, from_marks) = app.bulk_targets();
        assert!(from_marks);
        assert_eq!(targets.len(), 1);
        assert_eq!(app.hosts[targets[0]].id, marked);
    }

    #[test]
    fn with_nothing_marked_a_bulk_action_takes_what_the_filter_shows() {
        let mut app = test_app();
        app.query = "prod".into();
        app.recompute();
        let (targets, from_marks) = app.bulk_targets();
        assert!(!from_marks);
        assert_eq!(targets.len(), 2);
        assert!(
            targets
                .iter()
                .all(|&i| app.hosts[i].name.starts_with("prod")),
            "only the filtered hosts"
        );
    }

    #[test]
    fn binding_a_selection_points_the_hosts_at_one_credential_and_declares_it() {
        let mut app = test_app();
        app.query = "prod".into();
        app.recompute();
        let (targets, _) = app.bulk_targets();
        let req = BulkRequest {
            mode: BulkMode::Shared,
            credential: Some("corp-ldap".into()),
            secret: None,
            replace_existing: false,
        };
        let status = app.apply_bulk_secret(&req, &targets);
        assert!(status.contains("2 host(s) now use 'corp-ldap'"), "{status}");

        let bound: Vec<&str> = app
            .hosts
            .iter()
            .filter(|h| h.credential.as_deref() == Some("corp-ldap"))
            .map(|h| h.name.as_str())
            .collect();
        assert_eq!(bound.len(), 2);
        assert!(
            !bound.contains(&"bastion"),
            "an unselected host is untouched"
        );
        // Declared once, so it shows up in listings even before a secret exists.
        assert_eq!(app.credentials.len(), 1);
        assert_eq!(app.credentials[0].name, "corp-ldap");

        // Both now resolve to the same store key.
        let keys: std::collections::HashSet<String> = app
            .hosts
            .iter()
            .filter(|h| h.credential.is_some())
            .map(|h| h.secret_key())
            .collect();
        assert_eq!(keys.len(), 1);

        // And it round-tripped to disk as a v2 file.
        let text = std::fs::read_to_string(&app.hosts_path).unwrap();
        assert!(text.contains("format_version = 2"), "{text}");
        assert!(text.contains("[[credential]]"), "{text}");
    }

    #[test]
    fn binding_twice_does_not_declare_the_credential_twice() {
        let mut app = test_app();
        let req = BulkRequest {
            mode: BulkMode::Shared,
            credential: Some("corp-ldap".into()),
            secret: None,
            replace_existing: false,
        };
        app.apply_bulk_secret(&req, &[0]);
        app.apply_bulk_secret(&req, &[1]);
        assert_eq!(app.credentials.len(), 1);
        assert_eq!(app.hosts[0].credential.as_deref(), Some("corp-ldap"));
        assert_eq!(app.hosts[1].credential.as_deref(), Some("corp-ldap"));
    }

    #[test]
    fn unbinding_reports_only_the_hosts_that_were_bound() {
        let mut app = test_app();
        app.hosts[0].credential = Some("corp-ldap".into());
        let unbind = BulkRequest {
            mode: BulkMode::Unbind,
            credential: None,
            secret: None,
            replace_existing: false,
        };
        let status = app.apply_bulk_secret(&unbind, &[0, 1, 2]);
        assert!(status.starts_with("1 host(s) unbound"), "{status}");
        assert!(app.hosts.iter().all(|h| h.credential.is_none()));

        // Nothing bound: say so rather than claiming work.
        let status = app.apply_bulk_secret(&unbind, &[0, 1, 2]);
        assert!(status.contains("none of those hosts"), "{status}");
    }

    #[test]
    fn deleting_a_host_only_claims_a_secret_it_owns() {
        let mut hosts = vec![Host::new("a", "1.1.1.1"), Host::new("b", "1.1.1.2")];
        hosts[1].credential = Some("corp-ldap".into());
        let (own, shared) = (hosts[0].id.clone(), hosts[1].id.clone());

        assert_eq!(own_secret_key(&hosts, &own), Some(own.clone()));
        assert_eq!(
            own_secret_key(&hosts, &shared),
            None,
            "the other members still need the shared secret"
        );
        assert_eq!(own_secret_key(&hosts, "nobody"), None);
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
    fn the_pick_viewport_follows_the_flag_then_the_config() {
        // The flag wins over the config key.
        assert_eq!(
            pick_view_for(15, Some(8), false, 40),
            PickView::Inline(8),
            "--height overrides the config"
        );
        assert_eq!(pick_view_for(8, None, false, 40), PickView::Inline(8));
        // Either side can ask for the whole screen, and --full-screen beats a height.
        assert_eq!(pick_view_for(0, None, false, 40), PickView::FullScreen);
        assert_eq!(pick_view_for(15, Some(0), false, 40), PickView::FullScreen);
        assert_eq!(pick_view_for(15, Some(8), true, 40), PickView::FullScreen);
    }

    #[test]
    fn the_pick_viewport_always_leaves_the_prompt_a_row() {
        // Taller than the terminal is clamped, so the prompt the picker opened under cannot be
        // scrolled out of sight.
        assert_eq!(pick_view_for(15, Some(99), false, 24), PickView::Inline(23));
        assert_eq!(pick_view_for(15, None, false, 16), PickView::Inline(15));
        // Nothing useful fits below five rows, and an unreadable size takes the request as given.
        assert_eq!(pick_view_for(15, Some(1), false, 24), PickView::Inline(5));
        assert_eq!(pick_view_for(15, None, false, 0), PickView::Inline(15));
    }

    #[test]
    fn pick_mode_answers_only_the_chooser_keys() {
        for (code, ctrl) in [
            (KeyCode::Char('a'), false), // type to filter
            (KeyCode::Backspace, false),
            (KeyCode::Up, false),
            (KeyCode::Down, false),
            (KeyCode::Enter, false),
            (KeyCode::Esc, false),
            (KeyCode::Char('n'), true), // move
            (KeyCode::Char('p'), true),
            (KeyCode::Char('s'), true), // mark
            (KeyCode::Char('c'), true), // quit
        ] {
            assert!(pick_allows(code, ctrl), "{code:?} ctrl={ctrl} should pass");
        }
        // Everything that writes, connects sideways, or opens a full-screen screen is refused.
        for (code, ctrl) in [
            (KeyCode::Char('a'), true), // add
            (KeyCode::Char('e'), true), // edit
            (KeyCode::Char('d'), true), // delete
            (KeyCode::Char('k'), true), // bulk credentials
            (KeyCode::Char('o'), true), // import
            (KeyCode::Char('t'), true), // transfer
            (KeyCode::Char('f'), true), // forward
            (KeyCode::Char('y'), true), // yank
            (KeyCode::F(1), false),
            (KeyCode::F(2), false),
            (KeyCode::F(3), false),
            (KeyCode::F(4), false),
        ] {
            assert!(!pick_allows(code, ctrl), "{code:?} ctrl={ctrl} should not");
        }
    }

    #[test]
    fn pick_mode_cannot_edit_or_delete_the_inventory() {
        let mut app = test_app();
        app.pick = true;
        let before = app.hosts.len();
        for key in [
            ctrl(KeyCode::Char('d')),
            ctrl(KeyCode::Char('a')),
            ctrl(KeyCode::Char('e')),
            ctrl(KeyCode::Char('k')),
            ctrl(KeyCode::Char('o')),
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE),
        ] {
            app.on_key(key);
        }
        assert!(app.confirm.is_none(), "^d must not open a delete confirm");
        assert!(app.wizard.is_none(), "^a/^e must not open the form");
        assert!(app.bulk_secret.is_none());
        assert!(app.settings.is_none());
        assert!(app.sites_manager.is_none());
        assert_eq!(app.screen, Screen::List, "F1 must not open the overlay");
        assert_eq!(app.hosts.len(), before);
    }

    #[test]
    fn pick_mode_prints_the_chosen_row_and_connects_to_nothing() {
        let mut app = test_app();
        app.pick = true;
        assert!(app.picked_names().is_empty(), "nothing chosen yet");

        // Enter is the same key as a connect, routed differently.
        dispatch(&mut app, key(KeyCode::Enter));
        assert!(app.should_quit);
        assert_eq!(app.picked_names().len(), 1);
        // The cursor starts on the first row of the current order.
        let first = app.hosts[app.order[0]].name.clone();
        assert_eq!(app.picked_names(), vec![first]);
        // Nothing was recorded as used: the connect that follows does its own recording.
        assert!(app.state.stats.is_empty());
    }

    #[test]
    fn pick_mode_prints_every_marked_host_rather_than_the_cursor() {
        let mut app = test_app();
        app.pick = true;
        // Mark the second and third rows, leaving the cursor somewhere in the set.
        app.move_down();
        app.on_key(ctrl(KeyCode::Char('s')));
        app.move_down();
        app.on_key(ctrl(KeyCode::Char('s')));
        assert_eq!(app.marked.len(), 2);

        // A filter typed after marking cannot drop a mark from the output.
        app.query = "bastion".to_string();
        app.recompute();
        dispatch(&mut app, key(KeyCode::Enter));

        let names = app.picked_names();
        assert_eq!(names.len(), 2, "both marks print: {names:?}");
        for name in &names {
            let id = &app.hosts.iter().find(|h| &h.name == name).unwrap().id;
            assert!(app.is_marked(id), "{name} should be one of the marks");
        }
    }

    #[test]
    fn a_2fa_host_does_not_open_the_code_popup_in_pick_mode() {
        let mut app = test_app();
        app.pick = true;
        app.hosts[app.order[0]].requires_2fa = true;
        dispatch(&mut app, key(KeyCode::Enter));
        assert!(
            app.two_factor.is_none(),
            "a code would be stale before the shell used the name"
        );
        assert_eq!(app.picked_names().len(), 1);
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
    fn connecting_to_2fa_host_opens_code_popup_instead() {
        let mut app = test_app();
        let sel = app.order[app.selected];
        app.hosts[sel].requires_2fa = true;
        dispatch(&mut app, key(KeyCode::Enter));
        assert!(
            app.two_factor.is_some(),
            "a 2FA host should open the code popup"
        );
        assert!(!app.should_quit);
        assert!(app.pending_connect.is_none());
    }

    #[test]
    fn connecting_to_normal_host_skips_the_popup() {
        let mut app = test_app();
        dispatch(&mut app, key(KeyCode::Enter));
        assert!(app.two_factor.is_none());
        assert!(app.should_quit);
        assert!(app.pending_connect.is_some());
    }

    #[test]
    fn tmux_off_connects_in_place() {
        let mut app = test_app();
        app.config.tmux = Tmux::Off;
        app.in_tmux = true; // even inside tmux, "off" means off
        dispatch(&mut app, key(KeyCode::Enter));
        assert!(app.should_quit);
        assert!(app.pending_connect.is_some());
        assert!(app.connect_note.is_none());
    }

    #[test]
    fn tmux_mode_outside_tmux_connects_in_place() {
        // `tmux = "window"` on a machine that isn't running tmux must behave exactly as today.
        let mut app = test_app();
        app.config.tmux = Tmux::Window;
        app.in_tmux = false;
        dispatch(&mut app, key(KeyCode::Enter));
        assert!(app.should_quit);
        assert!(app.pending_connect.is_some());
        assert!(app.connect_note.is_none(), "no fallback note is owed here");
    }

    #[test]
    fn a_2fa_host_in_tmux_mode_falls_back_to_an_in_place_connect() {
        let mut app = test_app();
        app.config.tmux = Tmux::Window;
        app.in_tmux = true;
        app.hosts[1].requires_2fa = true;
        app.open_two_factor(1);
        for c in "654321".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        // No tmux window: the code would have had to ride tmux's argv.
        assert_eq!(app.pending_connect, Some(1));
        assert!(app.should_quit);
        assert_eq!(
            app.pending_2fa_code.as_ref().map(|c| c.as_str()),
            Some("654321")
        );
        let note = app.connect_note.expect("the skipped window is explained");
        assert!(note.starts_with("2FA host — connecting here"), "{note}");
    }

    #[test]
    fn two_factor_submit_queues_code_and_connects() {
        let mut app = test_app();
        app.open_two_factor(1);
        for c in "654321".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(app.two_factor.is_none());
        assert_eq!(app.pending_connect, Some(1));
        assert_eq!(
            app.pending_2fa_code.as_ref().map(|c| c.as_str()),
            Some("654321")
        );
        assert!(app.should_quit);
    }

    #[test]
    fn two_factor_esc_cancels_without_connecting() {
        let mut app = test_app();
        app.open_two_factor(0);
        app.on_key(key(KeyCode::Esc));
        assert!(app.two_factor.is_none());
        assert!(app.pending_connect.is_none());
        assert!(app.pending_2fa_code.is_none());
    }

    #[test]
    fn ctrl_f_opens_forward_popup_for_selected() {
        let mut app = test_app();
        let expected = app.order[app.selected];
        assert_eq!(
            app.on_key(ctrl(KeyCode::Char('f'))),
            Outcome::OpenForwardPopup(expected)
        );
    }

    #[test]
    fn forward_popup_captures_keys() {
        let mut app = test_app();
        app.open_forward_popup(0);
        assert!(app.forward_popup.is_some());
        // typing now goes to the popup, not the query
        app.on_key(key(KeyCode::Char('z')));
        assert!(app.query.is_empty());
        assert!(app.forward_popup.is_some());
        // esc closes it
        app.on_key(key(KeyCode::Esc));
        assert!(app.forward_popup.is_none());
    }

    #[test]
    fn f4_opens_forwards_manager() {
        let mut app = test_app();
        app.on_key(key(KeyCode::F(4)));
        assert!(app.forwards_manager.is_some());
        // typing routes to the manager, not the query
        app.on_key(key(KeyCode::Char('z')));
        assert!(app.query.is_empty());
        app.on_key(key(KeyCode::Esc));
        assert!(app.forwards_manager.is_none());
    }

    #[test]
    fn opening_the_manager_reconciles_dead_forwards() {
        use crate::forwards::{ForwardEntry, ForwardKind, ForwardSpec};
        let mut app = test_app();
        // A ledger entry pointing at a pid that is not our forward (here, pid 1 = launchd):
        // reconcile must drop it when the manager opens.
        app.forwards_state.forwards.push(ForwardEntry {
            id: "bogus".into(),
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
            pid: 1,
            started_at: 0,
        });
        app.on_key(key(KeyCode::F(4)));
        assert!(
            app.forwards_state.forwards.is_empty(),
            "dead forward should be reconciled away"
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
    fn the_parser_notes_reach_the_status_line_when_the_prefilled_form_closes() {
        let mut parsed = Host::new("web1", "10.0.0.7");
        parsed.user = Some("deploy".into());
        let notes = "dropped -v: verbose output is for one debugging run, not a saved host";

        // Saved: the notes follow the usual message.
        let mut app = test_app();
        app.wizard = Some(Wizard::prefill(&parsed, &[]));
        app.add_notes = Some(notes.to_string());
        app.on_key(ctrl(KeyCode::Char('s')));
        assert!(app.wizard.is_none());
        assert_eq!(
            app.status.as_deref(),
            Some(format!("host added · {notes}").as_str())
        );
        assert!(app.hosts.iter().any(|h| h.id == parsed.id));

        // Esc: nothing is added, and the notes still say what the line carried.
        let mut app = test_app();
        let before = app.hosts.len();
        app.wizard = Some(Wizard::prefill(&parsed, &[]));
        app.add_notes = Some(notes.to_string());
        app.on_key(key(KeyCode::Esc));
        assert!(app.wizard.is_none());
        assert_eq!(app.hosts.len(), before);
        assert_eq!(app.status.as_deref(), Some(notes));
        assert!(app.add_notes.is_none());
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
        let existing = HostsFile::with_parts(
            Vec::new(),
            Vec::new(),
            vec![Host::new("fromdisk", "9.9.9.9")],
        );
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
