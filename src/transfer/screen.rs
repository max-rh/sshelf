//! The dual-pane transfer screen: two [`Pane`]s (local + remote) over one [`TransferSession`].
//!
//! Key handling stays close to the rest of the app — `on_key` mutates state and returns an
//! outcome — but the screen also drains the worker's events each tick (the event loop polls
//! while this screen is open). Local navigation is synchronous (`std::fs`); remote navigation
//! sends a request and updates when the listing arrives.
//!
//! Keys: type to filter · `Tab` switch panes · `↑/↓` move · `Space` mark · `Ctrl-a` mark all ·
//! `→`/`Enter` open a dir (or send a file) · `Ctrl-s` send the marked entries (or the selected
//! one) to the other pane · `F7`/`Ctrl-f` create a directory · `←`/`Backspace` up · `Esc` cancel
//! a transfer, else clear marks, else clear the filter, else close.
//!
//! Sends run through a [`Queue`]: one transfer at a time (the worker's model), advancing on each
//! `Done`. An entry the destination already has is skipped without stopping the queue; a real
//! transfer failure stops it, because whatever broke will almost certainly break the rest too.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::model::Host;
use crate::ui::widgets::TextField;

use super::pane::{Pane, PaneEntry, Side, read_local_dir};
use super::worker::TransferSession;
use super::{Direction, Progress, TransferJob, WorkerCmd, WorkerEvent, target, validate_dir_name};

/// State of the one in-flight transfer.
struct Active {
    progress: Progress,
    /// e.g. `2 of 5  report.pdf → deploy@host`, shown on the progress line.
    label: String,
}

/// Why an entry was passed over instead of sent. Neither stops the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Skip {
    /// sshelf copies files and directories, not the links pointing at them.
    Symlink,
    /// v1 never overwrites (see `docs/transfer.md`).
    Exists,
}

impl Skip {
    /// The full explanation, used when a send was a single entry.
    fn message(self, name: &str) -> String {
        match self {
            Skip::Symlink => {
                format!("\"{name}\" is a symlink — skipped; send what it points at instead")
            }
            Skip::Exists => format!(
                "\"{name}\" already exists in the destination — skipped; rename or remove it there first"
            ),
        }
    }

    /// The short reason, used in a batch summary.
    fn short(self) -> &'static str {
        match self {
            Skip::Symlink => "symlink",
            Skip::Exists => "already there",
        }
    }
}

/// One entry queued for sending, captured when the send was requested so later navigation (or a
/// listing refresh) can't change what gets copied.
struct QueueItem {
    name: String,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
}

impl From<&PaneEntry> for QueueItem {
    fn from(e: &PaneEntry) -> Self {
        QueueItem {
            name: e.name.clone(),
            is_dir: e.is_dir,
            is_symlink: e.is_symlink,
            size: e.size,
        }
    }
}

/// A batch send in progress: the marked entries (or the single selection), moved one at a time.
struct Queue {
    direction: Direction,
    /// The pane that gains the files — refreshed once the queue drains.
    dest: Side,
    /// How the destination reads in the progress label (`local` or `user@host`).
    dest_label: String,
    src_dir: PathBuf,
    dest_dir: PathBuf,
    items: Vec<QueueItem>,
    /// Index of the item in flight (or the next one to consider).
    at: usize,
    skipped: Vec<(String, Skip)>,
}

impl Queue {
    /// Items after the one in flight — what a cancel or a failure leaves unsent.
    fn remaining(&self) -> usize {
        self.items.len().saturating_sub(self.at + 1)
    }
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
    queue: Option<Queue>,
    /// The inline "new directory" input, when open. It targets whichever pane has focus.
    mkdir: Option<TextField>,
    /// A directory just created remotely, to put the cursor on once its listing arrives.
    pending_select: Option<String>,
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
            queue: None,
            mkdir: None,
            pending_select: None,
        })
    }

    pub fn on_key(&mut self, key: KeyEvent) -> TransferOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Ctrl-c always closes, whatever else is open.
        if (key.code, ctrl) == (KeyCode::Char('c'), true) {
            return TransferOutcome::Close;
        }

        // The new-directory input owns the keyboard while it's up.
        if self.mkdir.is_some() {
            self.on_key_mkdir(key);
            return TransferOutcome::Continue;
        }

        // While a transfer runs, only cancel is live.
        if self.active.is_some() {
            if key.code == KeyCode::Esc {
                self.session.send(WorkerCmd::Cancel);
                self.status = Some("cancelling…".into());
            }
            return TransferOutcome::Continue;
        }

        self.status = None;
        match (key.code, ctrl) {
            (KeyCode::Esc, _) => {
                // One rung at a time: marks, then the filter, then the screen itself.
                if !self.focused().clear_marks() && !self.focused().clear_query() {
                    return TransferOutcome::Close;
                }
            }
            (KeyCode::Tab, _) => self.focus = self.other_side(),
            (KeyCode::Down, false) | (KeyCode::Char('n'), true) => self.focused().move_sel(1),
            (KeyCode::Up, false) | (KeyCode::Char('p'), true) => self.focused().move_sel(-1),
            (KeyCode::Enter, _) | (KeyCode::Right, false) => self.activate(),
            (KeyCode::Char('s'), true) => self.send(),
            (KeyCode::Char('a'), true) => self.mark_all(),
            // Marking wins over the filter for Space: a literal space is dropped from the
            // filter, and filenames containing one still match by the rest of their name.
            (KeyCode::Char(' '), false) => self.mark(),
            (KeyCode::F(7), _) | (KeyCode::Char('f'), true) => self.open_mkdir(),
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
                        if let Some(name) = self.pending_select.take() {
                            self.remote.select_name(&name);
                        }
                    }
                }
                WorkerEvent::Progress(p) => {
                    if let Some(active) = &mut self.active {
                        active.progress = p;
                    }
                }
                WorkerEvent::Done => {
                    self.active = None;
                    if let Some(q) = &mut self.queue {
                        q.at += 1;
                    }
                    self.start_next();
                }
                WorkerEvent::Cancelled => self.stop_queue("transfer cancelled".to_string()),
                WorkerEvent::MkdirDone(Ok(path)) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.status = Some(format!("created {name}/"));
                    if path.parent() == Some(self.remote.cwd.as_path()) {
                        self.request_remote();
                    }
                }
                WorkerEvent::MkdirDone(Err(e)) => {
                    self.pending_select = None;
                    self.status = Some(e);
                }
                WorkerEvent::Error(e) => {
                    if self.active.is_some() {
                        self.stop_queue(format!("transfer failed: {e}"));
                    } else {
                        // No transfer running, so it's a remote-listing failure.
                        self.remote.set_error(e);
                    }
                }
            }
        }
    }

    /// End the queue early (cancelled or failed), saying how much never went.
    fn stop_queue(&mut self, reason: String) {
        self.active = None;
        let Some(q) = self.queue.take() else {
            self.status = Some(reason);
            return;
        };
        self.status = Some(match q.remaining() {
            0 => reason,
            n => format!("{reason} — {n} queued item(s) not sent"),
        });
        self.refresh(q.dest);
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
            self.send();
        }
    }

    /// `Space`: mark or unmark the selected entry for a batch send.
    fn mark(&mut self) {
        if !self.focused().toggle_mark() {
            return;
        }
        let n = self.focused().marked_count();
        self.status = (n > 0).then(|| format!("{n} marked · ^s sends them"));
    }

    /// `Ctrl-a`: mark everything the filter shows — or clear every mark if it already does.
    fn mark_all(&mut self) {
        let n = self.focused().toggle_mark_all();
        self.status = Some(match n {
            0 => "marks cleared".to_string(),
            n => format!("{n} marked · ^s sends them"),
        });
    }

    /// `Ctrl-s`: send the marked entries — or, with none marked, the selected one — into the
    /// other pane's directory.
    fn send(&mut self) {
        if self.active.is_some() {
            self.status = Some("a transfer is already in progress — wait for it, or esc".into());
            return;
        }
        if self.connecting {
            self.status = Some("still connecting…".into());
            return;
        }
        let items = self.items_to_send();
        if items.is_empty() {
            return;
        }
        let (direction, dest) = match self.focus {
            Side::Local => (Direction::Upload, Side::Remote),
            Side::Remote => (Direction::Download, Side::Local),
        };
        self.queue = Some(Queue {
            direction,
            dest,
            dest_label: match dest {
                Side::Local => "local".to_string(),
                Side::Remote => self.target.clone(),
            },
            src_dir: self.focused().cwd.clone(),
            dest_dir: self.pane(dest).cwd.clone(),
            items,
            at: 0,
            skipped: Vec::new(),
        });
        // The queue is now the record of what's being sent; the marks have done their job.
        self.focused().clear_marks();
        self.start_next();
    }

    /// What a send should move: every marked entry, or the selection when nothing is marked.
    fn items_to_send(&self) -> Vec<QueueItem> {
        let pane = self.pane(self.focus);
        if pane.marked_count() > 0 {
            return pane.marked_entries().into_iter().map(Into::into).collect();
        }
        pane.selected_entry()
            .filter(|e| !e.is_parent())
            .map(|e| vec![e.into()])
            .unwrap_or_default()
    }

    /// Start the next queued item, stepping over anything that has to be skipped. Finishes the
    /// queue when nothing is left.
    fn start_next(&mut self) {
        loop {
            let Some(q) = &self.queue else { return };
            let Some(item) = q.items.get(q.at) else {
                return self.finish_queue();
            };
            let (name, is_dir, is_symlink, size) =
                (item.name.clone(), item.is_dir, item.is_symlink, item.size);
            let (dest, direction, total, at) = (q.dest, q.direction, q.items.len(), q.at);
            let src = q.src_dir.join(&name);
            let dest_dir = q.dest_dir.clone();
            let dest_label = q.dest_label.clone();

            // v1 never overwrites, and it copies files/directories rather than the links to
            // them. Either way the entry is passed over and the queue carries on.
            let skip = if is_symlink {
                Some(Skip::Symlink)
            } else if self.pane(dest).contains(&name) {
                Some(Skip::Exists)
            } else {
                None
            };
            if let Some(reason) = skip {
                if let Some(q) = &mut self.queue {
                    q.skipped.push((name, reason));
                    q.at += 1;
                }
                continue;
            }

            self.active = Some(Active {
                progress: Progress::default(),
                label: if total > 1 {
                    format!("{} of {total}  {name} → {dest_label}", at + 1)
                } else {
                    format!("{name} → {dest_label}")
                },
            });
            self.session.send(WorkerCmd::Transfer(TransferJob {
                direction,
                src,
                dest_dir,
                recursive: is_dir,
                size_hint: if is_dir { 0 } else { size },
            }));
            return;
        }
    }

    /// The queue drained: report what moved and refresh the destination once.
    fn finish_queue(&mut self) {
        let Some(q) = self.queue.take() else { return };
        let total = q.items.len();
        let sent = total - q.skipped.len();
        self.status = Some(if total == 1 {
            match q.skipped.first() {
                Some((name, reason)) => reason.message(name),
                None => "transfer complete".to_string(),
            }
        } else {
            let mut msg = format!("sent {sent} of {total}");
            if !q.skipped.is_empty() {
                let list: Vec<String> = q
                    .skipped
                    .iter()
                    .map(|(name, reason)| format!("{name} ({})", reason.short()))
                    .collect();
                msg.push_str(&format!(" · skipped {}", list.join(", ")));
            }
            msg
        });
        self.refresh(q.dest);
    }

    /// `F7` / `Ctrl-f`: open the inline "new directory" input on the focused pane.
    fn open_mkdir(&mut self) {
        if self.focus == Side::Remote && self.connecting {
            self.status =
                Some("still connecting — the remote directory isn't reachable yet".into());
            return;
        }
        self.status = None;
        self.mkdir = Some(TextField::new());
    }

    fn on_key_mkdir(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mkdir = None,
            KeyCode::Enter => self.submit_mkdir(),
            code => {
                if let Some(field) = self.mkdir.as_mut() {
                    field.handle(code);
                }
            }
        }
    }

    /// Create the typed directory in the focused pane's current directory. Never adopts an
    /// existing name, and never creates parents — one directory, right here (D-026).
    fn submit_mkdir(&mut self) {
        let Some(field) = &self.mkdir else { return };
        let name = field.value.trim().to_string();
        if let Err(e) = validate_dir_name(&name) {
            self.status = Some(e);
            return;
        }
        if self.pane(self.focus).contains(&name) {
            self.status = Some(format!(
                "\"{name}\" already exists here — pick another name, or esc to cancel"
            ));
            return;
        }
        self.mkdir = None;
        let dir = self.pane(self.focus).cwd.join(&name);
        match self.focus {
            Side::Local => match std::fs::create_dir(&dir) {
                Ok(()) => {
                    self.status = Some(format!("created {name}/"));
                    self.reload_local();
                    self.local.select_name(&name);
                }
                Err(e) => self.status = Some(format!("could not create {}: {e}", dir.display())),
            },
            Side::Remote => {
                self.pending_select = Some(name);
                self.status = Some("creating directory…".into());
                self.session.send(WorkerCmd::Mkdir(dir));
            }
        }
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

    /// Re-list one side (after a transfer lands or a directory is created).
    fn refresh(&mut self, side: Side) {
        match side {
            Side::Local => self.reload_local(),
            Side::Remote => self.request_remote(),
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

    fn pane(&self, side: Side) -> &Pane {
        match side {
            Side::Local => &self.local,
            Side::Remote => &self.remote,
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
    /// The text typed into the new-directory input, when it's open.
    pub fn mkdir_input(&self) -> Option<&TextField> {
        self.mkdir.as_ref()
    }

    /// Point one pane at `dir` and load it. The e2e tests browse throwaway directories the key
    /// bindings alone can't reach from the login directory.
    #[cfg(test)]
    pub(super) fn goto(&mut self, side: Side, dir: PathBuf) {
        match side {
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

    /// A screen with no worker behind it, for driving the queue / mkdir logic in tests: worker
    /// commands land in the returned receiver, and events are injected through the sender.
    #[cfg(test)]
    fn detached(
        local_dir: PathBuf,
        remote_entries: &[(&str, bool)],
    ) -> (
        Self,
        Receiver<WorkerCmd>,
        std::sync::mpsc::Sender<WorkerEvent>,
    ) {
        let (session, cmds) = TransferSession::detached();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let mut local = Pane::new(local_dir.clone());
        local.set_entries(read_local_dir(&local_dir).unwrap_or_default());
        let mut remote = Pane::new(PathBuf::from("/srv"));
        remote.set_entries(
            remote_entries
                .iter()
                .map(|&(name, is_dir)| PaneEntry {
                    name: name.to_string(),
                    is_dir,
                    is_symlink: false,
                    size: 4,
                })
                .collect(),
        );
        let screen = Self {
            target: "deploy@host".to_string(),
            session,
            events: event_rx,
            local,
            remote,
            focus: Side::Local,
            connecting: false,
            status: None,
            active: None,
            queue: None,
            mkdir: None,
            pending_select: None,
        };
        (screen, cmds, event_tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;
    use std::sync::mpsc::Sender;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    /// A local directory holding `sub/`, `a.txt`, `b.txt` — listed in that order (dirs first,
    /// then case-insensitive by name), behind the synthetic `..`.
    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sshelf-screen-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"aaa").unwrap();
        std::fs::write(dir.join("b.txt"), b"bbb").unwrap();
        dir
    }

    /// Drain the worker channel and return every transfer that was requested, by source name.
    fn sent_names(cmds: &Receiver<WorkerCmd>) -> Vec<String> {
        cmds.try_iter()
            .filter_map(|c| match c {
                WorkerCmd::Transfer(job) => {
                    Some(job.src.file_name()?.to_string_lossy().into_owned())
                }
                _ => None,
            })
            .collect()
    }

    /// Mark `sub/`, `a.txt` and `b.txt` in the local pane (rows 1..3, past the `..` entry).
    fn mark_all_three(screen: &mut TransferScreen) {
        for _ in 0..3 {
            screen.on_key(k(KeyCode::Down));
            screen.on_key(k(KeyCode::Char(' ')));
        }
        assert_eq!(screen.local.marked_count(), 3);
    }

    /// Feed one worker event and let the screen react, as the event loop would.
    fn deliver(screen: &mut TransferScreen, events: &Sender<WorkerEvent>, event: WorkerEvent) {
        events.send(event).unwrap();
        screen.drain_events();
    }

    #[test]
    fn a_batch_send_queues_every_mark_in_listing_order() {
        let dir = scratch();
        let (mut screen, cmds, events) = TransferScreen::detached(dir, &[]);
        mark_all_three(&mut screen);
        screen.on_key(ctrl(KeyCode::Char('s')));

        // One transfer at a time: only the first is in flight, and the marks are spent.
        assert_eq!(sent_names(&cmds), vec!["sub"]);
        assert_eq!(screen.local.marked_count(), 0);
        assert!(screen.active().unwrap().1.starts_with("1 of 3"));

        deliver(&mut screen, &events, WorkerEvent::Done);
        assert_eq!(sent_names(&cmds), vec!["a.txt"]);
        assert!(screen.active().unwrap().1.starts_with("2 of 3"));

        deliver(&mut screen, &events, WorkerEvent::Done);
        assert_eq!(sent_names(&cmds), vec!["b.txt"]);

        deliver(&mut screen, &events, WorkerEvent::Done);
        assert!(screen.active().is_none());
        assert_eq!(screen.status(), Some("sent 3 of 3"));
        // The destination is refreshed once, at the end.
        assert!(
            cmds.try_iter()
                .any(|c| matches!(c, WorkerCmd::ListRemote(_)))
        );
    }

    #[test]
    fn a_directory_is_queued_recursively_and_a_file_is_not() {
        let dir = scratch();
        let (mut screen, cmds, _events) = TransferScreen::detached(dir, &[]);
        screen.local.move_sel(1); // sub/
        screen.on_key(ctrl(KeyCode::Char('s')));
        match cmds.try_recv().unwrap() {
            WorkerCmd::Transfer(job) => {
                assert!(job.recursive);
                assert_eq!(job.size_hint, 0); // a directory's total isn't known up front
            }
            _ => panic!("expected a transfer"),
        }
    }

    #[test]
    fn a_skip_in_the_middle_does_not_abort_the_queue() {
        let dir = scratch();
        // The destination already holds `a.txt`, so that one is passed over — never overwritten.
        let (mut screen, cmds, events) = TransferScreen::detached(dir, &[("a.txt", false)]);
        mark_all_three(&mut screen);
        screen.on_key(ctrl(KeyCode::Char('s')));

        assert_eq!(sent_names(&cmds), vec!["sub"]);
        deliver(&mut screen, &events, WorkerEvent::Done);
        // `a.txt` was stepped over without a round trip; `b.txt` followed immediately.
        assert_eq!(sent_names(&cmds), vec!["b.txt"]);
        deliver(&mut screen, &events, WorkerEvent::Done);

        let status = screen.status().unwrap();
        assert!(status.starts_with("sent 2 of 3"), "{status}");
        assert!(status.contains("a.txt (already there)"), "{status}");
    }

    #[test]
    fn a_single_send_onto_an_existing_name_explains_itself_in_full() {
        let dir = scratch();
        let (mut screen, cmds, _events) = TransferScreen::detached(dir, &[("a.txt", false)]);
        screen.local.move_sel(2); // a.txt
        screen.on_key(ctrl(KeyCode::Char('s')));
        assert!(sent_names(&cmds).is_empty(), "nothing may be overwritten");
        let status = screen.status().unwrap();
        assert!(status.contains("\"a.txt\" already exists"), "{status}");
        assert!(status.contains("rename or remove"), "{status}");
    }

    #[test]
    fn a_transfer_failure_stops_the_rest_of_the_queue() {
        let dir = scratch();
        let (mut screen, cmds, events) = TransferScreen::detached(dir, &[]);
        mark_all_three(&mut screen);
        screen.on_key(ctrl(KeyCode::Char('s')));
        let _ = sent_names(&cmds);

        deliver(
            &mut screen,
            &events,
            WorkerEvent::Error("Permission denied".into()),
        );
        assert!(screen.active().is_none(), "the screen must not stay stuck");
        let status = screen.status().unwrap();
        // The underlying sftp text survives, and the user is told what didn't happen.
        assert!(status.contains("Permission denied"), "{status}");
        assert!(status.contains("2 queued item(s) not sent"), "{status}");
        assert!(sent_names(&cmds).is_empty());
    }

    #[test]
    fn cancelling_releases_the_screen_and_drops_the_queue() {
        let dir = scratch();
        let (mut screen, cmds, events) = TransferScreen::detached(dir, &[]);
        mark_all_three(&mut screen);
        screen.on_key(ctrl(KeyCode::Char('s')));
        let _ = sent_names(&cmds);

        screen.on_key(k(KeyCode::Esc)); // asks the worker to cancel
        assert!(
            cmds.try_iter().any(|c| matches!(c, WorkerCmd::Cancel)),
            "esc during a transfer must reach the worker"
        );
        deliver(&mut screen, &events, WorkerEvent::Cancelled);
        assert!(screen.active().is_none());
        let status = screen.status().unwrap();
        assert!(status.starts_with("transfer cancelled"), "{status}");
        assert!(status.contains("2 queued item(s) not sent"), "{status}");
    }

    #[test]
    fn ctrl_a_marks_the_filtered_view_then_clears() {
        let dir = scratch();
        let (mut screen, _cmds, _events) = TransferScreen::detached(dir, &[]);
        screen.on_key(k(KeyCode::Char('t'))); // filter to the .txt files
        screen.on_key(ctrl(KeyCode::Char('a')));
        assert_eq!(screen.local.marked_count(), 2);
        screen.on_key(ctrl(KeyCode::Char('a')));
        assert_eq!(screen.local.marked_count(), 0);
    }

    #[test]
    fn esc_clears_marks_then_the_filter_then_closes() {
        let dir = scratch();
        let (mut screen, _cmds, _events) = TransferScreen::detached(dir, &[]);
        screen.on_key(k(KeyCode::Char('t')));
        screen.on_key(ctrl(KeyCode::Char('a')));
        assert_eq!(screen.local.marked_count(), 2);

        assert!(matches!(
            screen.on_key(k(KeyCode::Esc)),
            TransferOutcome::Continue
        ));
        assert_eq!(screen.local.marked_count(), 0, "first esc drops the marks");
        assert_eq!(screen.local_pane().query(), "t", "…and keeps the filter");

        assert!(matches!(
            screen.on_key(k(KeyCode::Esc)),
            TransferOutcome::Continue
        ));
        assert_eq!(screen.local_pane().query(), "");

        assert!(matches!(
            screen.on_key(k(KeyCode::Esc)),
            TransferOutcome::Close
        ));
    }

    #[test]
    fn space_marks_instead_of_filtering() {
        let dir = scratch();
        let (mut screen, _cmds, _events) = TransferScreen::detached(dir, &[]);
        screen.local.move_sel(2); // a.txt
        screen.on_key(k(KeyCode::Char(' ')));
        assert_eq!(screen.local.marked_count(), 1);
        assert_eq!(
            screen.local_pane().query(),
            "",
            "space never reaches the filter"
        );
    }

    #[test]
    fn f7_and_ctrl_f_both_open_the_new_directory_input() {
        let dir = scratch();
        for open in [k(KeyCode::F(7)), ctrl(KeyCode::Char('f'))] {
            let (mut screen, _cmds, _events) = TransferScreen::detached(dir.clone(), &[]);
            screen.on_key(open);
            assert!(screen.mkdir_input().is_some());
            screen.on_key(k(KeyCode::Esc));
            assert!(screen.mkdir_input().is_none(), "esc cancels the input");
        }
    }

    #[test]
    fn creating_a_local_directory_refreshes_and_selects_it() {
        let dir = scratch();
        let (mut screen, _cmds, _events) = TransferScreen::detached(dir.clone(), &[]);
        screen.on_key(k(KeyCode::F(7)));
        for c in "releases".chars() {
            screen.on_key(k(KeyCode::Char(c)));
        }
        screen.on_key(k(KeyCode::Enter));

        assert!(dir.join("releases").is_dir(), "the directory is on disk");
        assert!(screen.mkdir_input().is_none());
        assert_eq!(screen.status(), Some("created releases/"));
        assert_eq!(
            screen.local_pane().selected_entry().unwrap().name,
            "releases",
            "the new directory is under the cursor"
        );
    }

    #[test]
    fn creating_a_local_directory_never_adopts_an_existing_one() {
        let dir = scratch();
        let (mut screen, _cmds, _events) = TransferScreen::detached(dir.clone(), &[]);
        screen.on_key(k(KeyCode::F(7)));
        for c in "sub".chars() {
            screen.on_key(k(KeyCode::Char(c)));
        }
        screen.on_key(k(KeyCode::Enter));
        let status = screen.status().unwrap();
        assert!(status.contains("already exists here"), "{status}");
        assert!(
            screen.mkdir_input().is_some(),
            "the input stays open so the name can be fixed"
        );
    }

    #[test]
    fn a_rejected_directory_name_keeps_the_input_open() {
        let dir = scratch();
        let (mut screen, _cmds, _events) = TransferScreen::detached(dir.clone(), &[]);
        screen.on_key(k(KeyCode::F(7)));
        for c in "a/b".chars() {
            screen.on_key(k(KeyCode::Char(c)));
        }
        screen.on_key(k(KeyCode::Enter));
        assert!(screen.status().unwrap().contains("not a path"));
        assert!(screen.mkdir_input().is_some());
        assert!(!dir.join("a").exists(), "nothing was created");
    }

    #[test]
    fn creating_a_remote_directory_goes_through_the_worker() {
        let dir = scratch();
        let (mut screen, cmds, events) = TransferScreen::detached(dir, &[("logs", true)]);
        screen.on_key(k(KeyCode::Tab)); // focus the remote pane
        screen.on_key(k(KeyCode::F(7)));
        for c in "releases".chars() {
            screen.on_key(k(KeyCode::Char(c)));
        }
        screen.on_key(k(KeyCode::Enter));
        match cmds.try_recv().unwrap() {
            WorkerCmd::Mkdir(path) => assert_eq!(path, PathBuf::from("/srv/releases")),
            _ => panic!("expected a remote mkdir"),
        }

        // The reply refreshes the listing, and the new directory lands under the cursor.
        deliver(
            &mut screen,
            &events,
            WorkerEvent::MkdirDone(Ok(PathBuf::from("/srv/releases"))),
        );
        assert_eq!(screen.status(), Some("created releases/"));
        assert!(
            cmds.try_iter()
                .any(|c| matches!(c, WorkerCmd::ListRemote(_)))
        );
        deliver(
            &mut screen,
            &events,
            WorkerEvent::Listing {
                path: PathBuf::from("/srv"),
                entries: vec![
                    crate::transfer::RemoteEntry {
                        name: "logs".into(),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                    },
                    crate::transfer::RemoteEntry {
                        name: "releases".into(),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                    },
                ],
            },
        );
        assert_eq!(
            screen.remote_pane().selected_entry().unwrap().name,
            "releases"
        );
    }

    #[test]
    fn a_failed_remote_mkdir_reports_sftp_and_keeps_the_listing() {
        let dir = scratch();
        let (mut screen, _cmds, events) = TransferScreen::detached(dir, &[("logs", true)]);
        deliver(
            &mut screen,
            &events,
            WorkerEvent::MkdirDone(Err("could not create /srv/x: Permission denied".into())),
        );
        assert_eq!(
            screen.status(),
            Some("could not create /srv/x: Permission denied")
        );
        // A mkdir failure is not a listing failure — the pane keeps showing the directory.
        assert!(screen.remote_pane().error.is_none());
        assert!(!screen.remote_pane().rows().is_empty());
    }

    #[test]
    fn skip_messages_name_the_entry_and_the_way_out() {
        let exists = Skip::Exists.message("report.pdf");
        assert!(exists.contains("report.pdf"));
        assert!(exists.contains("already exists"));
        assert!(exists.contains("rename or remove"));
        let link = Skip::Symlink.message("latest");
        assert!(link.contains("latest") && link.contains("symlink"));
    }

    fn queue_of(names: &[&str]) -> Queue {
        Queue {
            direction: Direction::Upload,
            dest: Side::Remote,
            dest_label: "deploy@host".into(),
            src_dir: PathBuf::from("/src"),
            dest_dir: PathBuf::from("/dst"),
            items: names
                .iter()
                .map(|n| QueueItem {
                    name: (*n).to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 1,
                })
                .collect(),
            at: 0,
            skipped: Vec::new(),
        }
    }

    #[test]
    fn remaining_counts_only_what_is_still_unsent() {
        let mut q = queue_of(&["a", "b", "c"]);
        assert_eq!(q.remaining(), 2); // `a` is in flight
        q.at = 2;
        assert_eq!(q.remaining(), 0);
        // Past the end (the queue just drained) must not underflow.
        q.at = 3;
        assert_eq!(q.remaining(), 0);
    }
}
