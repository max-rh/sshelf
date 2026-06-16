//! The dual-pane transfer screen: two [`Pane`]s (local + remote) over one [`TransferSession`].
//!
//! Key handling stays close to the rest of the app — `on_key` mutates state and returns an
//! outcome — but the screen also drains the worker's events each tick (the event loop polls
//! while this screen is open). Local navigation is synchronous (`std::fs`); remote navigation
//! sends a request and updates when the listing arrives.
//!
//! Keys: type to filter · `Tab` switch panes · `↑/↓` move · `→`/`Enter` open a dir (or send a
//! file) · `Ctrl-s` send the selection (file or folder) to the other pane · `←`/`Backspace` up
//! · `Esc` cancel a transfer, else clear the filter, else close.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::model::Host;

use super::pane::{Pane, Side, read_local_dir};
use super::worker::TransferSession;
use super::{Direction, Progress, TransferJob, WorkerCmd, WorkerEvent, target};

/// State of the one in-flight transfer.
struct Active {
    progress: Progress,
    /// Which pane to refresh once it completes.
    dest: Side,
    /// e.g. `report.pdf → deploy@host`, shown on the progress line.
    label: String,
}

/// What the app should do after the screen handled a key.
pub enum TransferOutcome {
    Continue,
    Close,
}

pub struct TransferScreen {
    /// `user@host`, for the remote pane's title.
    target: String,
    session: TransferSession,
    events: Receiver<WorkerEvent>,
    local: Pane,
    remote: Pane,
    focus: Side,
    /// The master is still being established; the remote pane shows "connecting…".
    connecting: bool,
    status: Option<String>,
    active: Option<Active>,
}

impl TransferScreen {
    /// Open the screen for `host`, spawning the worker and loading the local pane at `start`.
    /// The remote pane fills in once the master reports its working directory.
    pub fn open(host: &Host, has_secret: bool, start: PathBuf) -> std::io::Result<Self> {
        let (session, events) = TransferSession::spawn(host.clone(), has_secret)?;
        let mut local = Pane::new(start.clone());
        match read_local_dir(&start) {
            Ok(entries) => local.set_entries(entries),
            Err(e) => local.set_error(e),
        }
        Ok(Self {
            target: target(host),
            session,
            events,
            local,
            // Placeholder until WorkerEvent::Ready delivers the remote home directory.
            remote: Pane::new(PathBuf::from("/")),
            focus: Side::Local,
            connecting: true,
            status: None,
            active: None,
        })
    }

    pub fn on_key(&mut self, key: KeyEvent) -> TransferOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // While a transfer runs, only cancel / close are live.
        if self.active.is_some() {
            match (key.code, ctrl) {
                (KeyCode::Esc, _) => {
                    self.session.send(WorkerCmd::Cancel);
                    self.status = Some("cancelling…".into());
                }
                (KeyCode::Char('c'), true) => return TransferOutcome::Close,
                _ => {}
            }
            return TransferOutcome::Continue;
        }

        self.status = None;
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) => return TransferOutcome::Close,
            (KeyCode::Esc, _) => {
                if !self.focused().clear_query() {
                    return TransferOutcome::Close;
                }
            }
            (KeyCode::Tab, _) => self.focus = self.other_side(),
            (KeyCode::Down, false) | (KeyCode::Char('n'), true) => self.focused().move_sel(1),
            (KeyCode::Up, false) | (KeyCode::Char('p'), true) => self.focused().move_sel(-1),
            (KeyCode::Enter, _) | (KeyCode::Right, false) => self.activate(),
            (KeyCode::Char('s'), true) => self.transfer_selected(),
            (KeyCode::Left, false) => self.go_up(),
            (KeyCode::Backspace, _) => {
                if !self.focused().pop_query() {
                    self.go_up();
                }
            }
            (KeyCode::Char(c), false) => self.focused().push_query(c),
            _ => {}
        }
        TransferOutcome::Continue
    }

    /// Apply any pending worker events. Called once per event-loop tick.
    pub fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                WorkerEvent::Ready(Ok(home)) => {
                    self.connecting = false;
                    self.remote.navigate_to(home);
                    self.request_remote();
                }
                WorkerEvent::Ready(Err(e)) => {
                    self.connecting = false;
                    self.remote.set_error(format!("connection failed: {e}"));
                }
                WorkerEvent::Listing { path, entries } => {
                    // Ignore a listing for a directory we've since navigated away from.
                    if path == self.remote.cwd {
                        self.remote
                            .set_entries(entries.into_iter().map(Into::into).collect());
                    }
                }
                WorkerEvent::Progress(p) => {
                    if let Some(active) = &mut self.active {
                        active.progress = p;
                    }
                }
                WorkerEvent::Done => {
                    let dest = self.active.take().map(|a| a.dest);
                    self.status = Some("transfer complete".into());
                    match dest {
                        Some(Side::Local) => self.reload_local(),
                        Some(Side::Remote) => self.request_remote(),
                        None => {}
                    }
                }
                WorkerEvent::Error(e) => {
                    if self.active.take().is_some() {
                        self.status = Some(format!("transfer failed: {e}"));
                    } else {
                        // No transfer running, so it's a remote-listing failure.
                        self.remote.set_error(e);
                    }
                }
            }
        }
    }

    /// `Enter`/`→`: descend into a directory, go up on `..`, or send a plain file.
    fn activate(&mut self) {
        let Some((is_parent, name, is_dir, is_symlink)) = self
            .focused()
            .selected_entry()
            .map(|e| (e.is_parent(), e.name.clone(), e.is_dir, e.is_symlink))
        else {
            return;
        };
        if is_parent {
            self.go_up();
        } else if is_dir && !is_symlink {
            let dir = self.focused().cwd.join(&name);
            self.navigate(dir);
        } else {
            self.transfer_selected();
        }
    }

    /// `Ctrl-s`: send the selected file or folder into the other pane's directory.
    fn transfer_selected(&mut self) {
        if self.active.is_some() {
            self.status = Some("a transfer is already in progress".into());
            return;
        }
        if self.connecting {
            self.status = Some("still connecting…".into());
            return;
        }
        let Some((is_parent, name, is_dir, is_symlink, size)) =
            self.focused().selected_entry().map(|e| {
                (
                    e.is_parent(),
                    e.name.clone(),
                    e.is_dir,
                    e.is_symlink,
                    e.size,
                )
            })
        else {
            return;
        };
        if is_parent {
            return;
        }
        if is_symlink {
            self.status = Some(format!("\"{name}\" is a symlink — skipped in this version"));
            return;
        }
        // v1 doesn't overwrite: if the destination already has this name, skip with a message.
        if self.other().contains(&name) {
            self.status = Some(format!(
                "\"{name}\" already exists in the destination — skipped"
            ));
            return;
        }

        let src = self.focused().cwd.join(&name);
        let dest_dir = self.other().cwd.clone();
        let (direction, dest) = match self.focus {
            Side::Local => (Direction::Upload, Side::Remote),
            Side::Remote => (Direction::Download, Side::Local),
        };
        let dest_label = match dest {
            Side::Local => "local".to_string(),
            Side::Remote => self.target.clone(),
        };
        self.active = Some(Active {
            progress: Progress::default(),
            dest,
            label: format!("{name} → {dest_label}"),
        });
        self.session.send(WorkerCmd::Transfer(TransferJob {
            direction,
            src,
            dest_dir,
            recursive: is_dir,
            size_hint: if is_dir { 0 } else { size },
        }));
    }

    fn navigate(&mut self, dir: PathBuf) {
        match self.focus {
            Side::Local => {
                self.local.navigate_to(dir);
                self.reload_local();
            }
            Side::Remote => {
                self.remote.navigate_to(dir);
                self.request_remote();
            }
        }
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.focused().parent() {
            self.navigate(parent);
        }
    }

    fn reload_local(&mut self) {
        match read_local_dir(&self.local.cwd) {
            Ok(entries) => self.local.set_entries(entries),
            Err(e) => self.local.set_error(e),
        }
    }

    fn request_remote(&self) {
        self.session
            .send(WorkerCmd::ListRemote(self.remote.cwd.clone()));
    }

    fn focused(&mut self) -> &mut Pane {
        match self.focus {
            Side::Local => &mut self.local,
            Side::Remote => &mut self.remote,
        }
    }

    fn other(&self) -> &Pane {
        match self.focus {
            Side::Local => &self.remote,
            Side::Remote => &self.local,
        }
    }

    fn other_side(&self) -> Side {
        match self.focus {
            Side::Local => Side::Remote,
            Side::Remote => Side::Local,
        }
    }

    // Read-only accessors for the renderer.
    pub fn target(&self) -> &str {
        &self.target
    }
    pub fn local_pane(&self) -> &Pane {
        &self.local
    }
    pub fn remote_pane(&self) -> &Pane {
        &self.remote
    }
    pub fn focused_side(&self) -> Side {
        self.focus
    }
    pub fn is_connecting(&self) -> bool {
        self.connecting
    }
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }
    /// The in-flight transfer's progress and label, if one is running.
    pub fn active(&self) -> Option<(Progress, &str)> {
        self.active.as_ref().map(|a| (a.progress, a.label.as_str()))
    }
}
