//! The transfer worker thread and the `ssh` ControlMaster it owns.
//!
//! The TUI event loop is synchronous; this background thread runs the blocking `ssh`/`sftp` so
//! a slow link never freezes the UI. They talk over std channels: the UI sends
//! [`WorkerCmd`], the worker emits [`WorkerEvent`] (drained each tick). The worker owns the
//! master child, the private directory its control socket lives in, and every short-lived
//! `sftp` it starts; it tears all of them down when it stops — on `Shutdown`, a dropped command
//! channel, or a failed handshake. Nothing here may outlive the screen: the UI thread has a
//! terminal to restore, so every child can be stopped on demand — a long transfer by a cancel
//! or a shutdown, everything else by that or its own deadline — and teardown never blocks.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs::DirBuilder;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ulid::Ulid;

use crate::model::Host;
use crate::ssh;

use super::{
    Direction, Progress, RemoteEntry, TransferJob, WorkerCmd, WorkerEvent, master_args,
    master_check_args, master_exit_args, sftp_batch_args, shell_quote, target,
};

/// Wait this long for the master to authenticate and come up before giving up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Poll cadence while waiting on a child (handshake readiness, transfer progress, cancel).
const POLL: Duration = Duration::from_millis(100);
/// Emit a progress event roughly this often (every Nth poll) to avoid UI jitter.
const PROGRESS_EVERY: u32 = 5;
/// Give a remote listing this long before the `sftp` running it is killed. A directory walk on
/// a slow link is legitimately slow; a server that never answers is not.
const LIST_TIMEOUT: Duration = Duration::from_secs(60);
/// The same ceiling for a `mkdir` or a `pwd` — one round trip, not a walk.
const MKDIR_TIMEOUT: Duration = Duration::from_secs(30);
/// Poll cadence while a short-lived batch runs. Tighter than [`POLL`]: closing the screen waits
/// on this loop noticing, and the terminal comes back only afterwards.
const BATCH_POLL: Duration = Duration::from_millis(50);
/// Keep at most this much of one batch's stdout — a listing is text, and a server that answers
/// with gigabytes of it is not one to allocate for.
const STDOUT_CAP: usize = 16 * 1024 * 1024;
/// …and this much of its stderr, of which only the last line is ever shown.
const STDERR_CAP: usize = 64 * 1024;
/// Parse at most this many entries out of one listing. Past this the pane is unusable anyway,
/// and the screen says the listing was cut rather than passing it off as complete.
pub(super) const MAX_ENTRIES: usize = 50_000;
/// Give `ssh -O check` / `ssh -O exit` this long. Both are a round trip to a socket on this
/// machine: anything slower is a master that is not answering, and teardown holds the closing
/// screen in the alternate screen until it returns.
const MUX_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a closing screen waits for the worker to confirm the master is gone.
const SHUTDOWN_ACK: Duration = Duration::from_secs(2);
/// AF_UNIX paths are capped near 104 bytes; stay clear of that with room for the socket name.
const MAX_SOCKET_PATH: usize = 100;
/// How many names to try before giving up on a free session directory.
const SESSION_DIR_TRIES: u32 = 8;

/// Optional transfer diagnostics, enabled by the `SSHELF_TRANSFER_LOG` file path (or
/// `--transfer-log`). Records the `ssh`/`sftp` commands, the local and remote paths they touch,
/// their stderr, and every value the host's `extra_args` contributes — no secret, since the
/// password reaches `ssh` via `SSH_ASKPASS` and never argv, but enough to describe the
/// connection in full, so the file wants a private home. Lives on the single worker thread, so
/// a `RefCell` suffices.
struct DebugLog(Option<RefCell<std::fs::File>>);

impl DebugLog {
    fn from_env() -> Self {
        let path = std::env::var_os(super::LOG_ENV).filter(|p| !p.is_empty());
        DebugLog(path.and_then(|p| open_log(Path::new(&p))).map(RefCell::new))
    }

    fn log(&self, msg: &str) {
        if let Some(file) = &self.0 {
            let _ = writeln!(file.borrow_mut(), "{msg}");
        }
    }
}

/// Open the user's transfer log for appending: mode `0600` on create (the umask is not a
/// permission policy), and `O_NOFOLLOW`, so a symlink someone left at that path can neither
/// redirect the log nor have this truncate what it points at.
#[cfg(unix)]
fn open_log(path: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => Some(file),
        Err(_) => {
            // Worth one line when the symlink guard is what refused: the user asked for a log
            // and is getting none, and a silent refusal looks like the flag did nothing. The
            // check is only for the wording — the open already declined to follow anything.
            if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                eprintln!(
                    "sshelf: {} is a symlink — not writing the transfer log there",
                    path.display()
                );
            }
            None
        }
    }
}

#[cfg(not(unix))]
fn open_log(path: &Path) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// A handle to the running transfer worker. Dropping it asks the worker to shut the master
/// down and waits briefly for it to confirm — never longer, because the UI thread still has a
/// terminal to restore and a stuck server must not be able to hold that up.
pub struct TransferSession {
    cmd_tx: Sender<WorkerCmd>,
    /// The worker sends here once the master and its socket are gone. Waited on (briefly) by
    /// `Drop` in place of joining the thread.
    ack: Receiver<()>,
}

impl TransferSession {
    /// Spawn the worker for `host` (`has_secret` decides whether to wire `SSH_ASKPASS`). The
    /// master connection is opened on the worker thread; the first event is a
    /// [`WorkerEvent::Ready`] reporting whether it came up. Returns the handle plus the channel
    /// of events to drain in the UI loop.
    pub fn spawn(host: Host, has_secret: bool) -> std::io::Result<(Self, Receiver<WorkerEvent>)> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let (ack_tx, ack) = mpsc::channel();
        // The join handle is dropped on purpose: nothing ever joins this thread (see `Drop`).
        std::thread::Builder::new()
            .name("sshelf-transfer".into())
            .spawn(move || run(host, has_secret, cmd_rx, event_tx, ack_tx))?;
        Ok((Self { cmd_tx, ack }, event_rx))
    }

    /// Queue a command for the worker. A send error means the worker already stopped, in which
    /// case the screen is closing anyway, so it's ignored.
    pub fn send(&self, cmd: WorkerCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// A session with no `ssh` behind it: commands land in the returned receiver instead. Lets
    /// the screen's queue / mkdir logic be driven without a server (the real transport is
    /// covered by `e2e.rs`).
    #[cfg(test)]
    pub(super) fn detached() -> (Self, Receiver<WorkerCmd>) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        // The sender goes out of scope here, so `Drop`'s wait returns at once.
        let (_ack_tx, ack) = mpsc::channel();
        (Self { cmd_tx, ack }, cmd_rx)
    }
}

impl Drop for TransferSession {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerCmd::Shutdown);
        // Wait for the worker to say the master is down — but only for a moment. A hostile or
        // hung server must never be able to keep the UI in the alternate screen; if the worker
        // is late, the thread finishes teardown on its own while the terminal comes back.
        let _ = self.ack.recv_timeout(SHUTDOWN_ACK);
    }
}

/// A `DirBuilder` that creates exactly one directory, mode `0700`. Never its parents: an
/// existing name has to be an error, not something to adopt.
#[cfg(unix)]
fn private_dir_builder() -> DirBuilder {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder
}

#[cfg(not(unix))]
fn private_dir_builder() -> DirBuilder {
    DirBuilder::new()
}

/// Create `dir` with mode `0700`, treating "already there" as success. The same helper the
/// config and data directories go through, with `enforce_mode` off: only a directory sshelf
/// creates gets those permissions — chmod-ing one somebody else made (`$XDG_RUNTIME_DIR` above
/// all) would be a rude surprise, and it is already theirs to set.
fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    crate::paths::ensure_private_dir(dir, false)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))
}

/// The private directory the per-session mux directories live in: `$XDG_RUNTIME_DIR/sshelf`
/// when the runtime dir is set and real (it is already per-user and short, which an AF_UNIX
/// path needs), otherwise `run/` under sshelf's own data directory.
fn session_parent() -> Result<PathBuf, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    session_parent_in(runtime.as_deref(), || {
        crate::paths::Paths::resolve()
            .map(|p| p.data_dir)
            .map_err(|e| format!("could not find the sshelf data directory: {e}"))
    })
}

/// The same with the inputs handed in, so the tests can take either branch without touching
/// the process environment — which is process-global and, in edition 2024, unsafe to mutate
/// from a test thread. `data` stays a closure: resolving the data directory can fail, and it
/// has no business failing a session that is going to use the runtime directory anyway.
fn session_parent_in(
    runtime: Option<&Path>,
    data: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<PathBuf, String> {
    if let Some(runtime) = runtime.filter(|r| r.is_dir()) {
        let dir = runtime.join("sshelf");
        ensure_private_dir(&dir)?;
        return Ok(dir);
    }
    let data = data()?;
    // The XDG data root belongs to the user, not to us: make it the ordinary way, and claim
    // only sshelf's own two levels below it.
    if let Some(root) = data.parent() {
        std::fs::create_dir_all(root).map_err(|e| format!("creating {}: {e}", root.display()))?;
    }
    ensure_private_dir(&data)?;
    let dir = data.join("run");
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Create a fresh `mux-<ulid>` directory inside `parent`. `create` rather than `create_dir_all`
/// is the point: a name that already exists is a name someone else got to first, so take
/// another one instead of moving in.
fn create_session_dir(parent: &Path) -> Result<PathBuf, String> {
    for _ in 0..SESSION_DIR_TRIES {
        let dir = parent.join(format!("mux-{}", Ulid::new()));
        match private_dir_builder().create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not create {}: {e}", dir.display())),
        }
    }
    Err(format!(
        "could not find a free session directory under {}",
        parent.display()
    ))
}

/// The master's control socket: `m.sock` inside a per-session directory sshelf creates with
/// mode `0700`.
///
/// A predictable path in `/tmp` is not good enough. Another local account can pre-create one,
/// which alone stalls the handshake until it times out, and on an OpenSSH build that doesn't
/// check socket ownership a process sitting there can answer as the mux and receive the `sftp`
/// ride commands. A directory nobody else can write into removes both. Socket and directory
/// go away together on teardown.
struct ControlSocket {
    /// The session directory, removed along with the socket.
    dir: PathBuf,
    path: PathBuf,
}

impl ControlSocket {
    fn new() -> Result<Self, String> {
        Self::new_in(&session_parent()?)
    }

    /// Same, with the parent given. Split out so the tests can point it at a scratch directory
    /// rather than the user's runtime or data directory.
    fn new_in(parent: &Path) -> Result<Self, String> {
        let dir = create_session_dir(parent)?;
        let socket = Self {
            path: dir.join("m.sock"),
            dir,
        };
        // Falling back to /tmp when the path is too long would give up exactly the protection
        // this type exists for, so say so instead and let the user pick somewhere shorter.
        if socket.path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(format!(
                "the control socket path is too long for a unix socket: {} — set XDG_RUNTIME_DIR to a shorter directory",
                socket.path.display()
            ));
        }
        Ok(socket)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The socket must not exist yet. The session directory is fresh and private, so anything
    /// sitting at this path is not a mux sshelf opened — talking to it would hand the transfer
    /// to whoever put it there.
    fn ensure_absent(&self) -> Result<(), String> {
        // symlink_metadata: a symlink counts as "already there", dangling or not.
        if std::fs::symlink_metadata(&self.path).is_ok() {
            return Err(format!(
                "{} already exists — refusing to reuse a control socket sshelf did not create",
                self.path.display()
            ));
        }
        Ok(())
    }

    /// Remove the socket and the session directory. Run on the worker's normal shutdown path as
    /// well as from `Drop`, so a clean exit doesn't depend on drop order; both are safe twice.
    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// What a mid-child check of the command channel found.
enum Stop {
    /// The screen is closing (`Shutdown`, or its end of the channel is gone).
    Shutdown,
    /// The user asked to cancel what is running.
    Cancel,
}

/// The worker's end of the command channel, with somewhere to park commands that arrive while a
/// child is running.
///
/// A running child has to be interruptible, so the channel is polled mid-flight — but a poll
/// takes whatever is at the front, and that may well be the next directory to list. Dropping it
/// would leave that pane loading forever, so anything that isn't a stop request waits here and
/// is served in order once the child is done.
struct Commands {
    rx: Receiver<WorkerCmd>,
    deferred: RefCell<VecDeque<WorkerCmd>>,
}

impl Commands {
    fn new(rx: Receiver<WorkerCmd>) -> Self {
        Commands {
            rx,
            deferred: RefCell::new(VecDeque::new()),
        }
    }

    /// The next command to serve, blocking until one arrives. `None` once the screen is gone.
    fn recv(&self) -> Option<WorkerCmd> {
        if let Some(cmd) = self.deferred.borrow_mut().pop_front() {
            return Some(cmd);
        }
        self.rx.recv().ok()
    }

    /// Look for a stop request without blocking, parking anything else for later.
    ///
    /// A cancel also throws away any transfer parked here. The screen sends the destination
    /// listing and the transfer back to back, so the transfer the user is cancelling may well
    /// be sitting in `deferred` behind the listing that is running right now — serving it
    /// afterwards would move exactly the bytes the cancel stopped. Listings and mkdirs are
    /// separate requests and still get served.
    fn poll_stop(&self) -> Option<Stop> {
        loop {
            match self.rx.try_recv() {
                Ok(WorkerCmd::Shutdown) | Err(TryRecvError::Disconnected) => {
                    return Some(Stop::Shutdown);
                }
                Ok(WorkerCmd::Cancel) => {
                    self.deferred
                        .borrow_mut()
                        .retain(|cmd| !matches!(cmd, WorkerCmd::Transfer(_)));
                    return Some(Stop::Cancel);
                }
                Ok(other) => self.deferred.borrow_mut().push_back(other),
                Err(TryRecvError::Empty) => return None,
            }
        }
    }
}

/// The worker thread body: open the master, serve commands, then tear everything down and say
/// so on `ack`. Every early return drops `ack`, which the screen reads as "nothing to wait for".
fn run(
    host: Host,
    has_secret: bool,
    cmd_rx: Receiver<WorkerCmd>,
    events: Sender<WorkerEvent>,
    ack: Sender<()>,
) {
    let target = target(&host);
    let dbg = DebugLog::from_env();
    let socket = match ControlSocket::new().and_then(|s| s.ensure_absent().map(|()| s)) {
        Ok(socket) => socket,
        Err(e) => {
            // Report it: a screen that never hears `Ready` just sits on "connecting…".
            dbg.log(&format!("control socket: {e}"));
            let _ = events.send(WorkerEvent::Ready(Err(e)));
            return;
        }
    };
    dbg.log(&format!(
        "=== transfer session: {target} (askpass wired: {has_secret}) ===",
    ));
    dbg.log(&format!(
        "$ ssh {}",
        master_args(&host, socket.path(), has_secret).join(" ")
    ));

    let mut master = match open_master(&host, has_secret, socket.path()) {
        Ok(child) => child,
        Err(e) => {
            dbg.log(&format!("could not launch ssh: {e}"));
            let _ = events.send(WorkerEvent::Ready(Err(format!(
                "could not launch ssh: {e}"
            ))));
            return;
        }
    };

    let cmds = Commands::new(cmd_rx);
    match handshake(&socket, &target, &mut master, &cmds) {
        Ok(()) => serve(&socket, &target, &cmds, &events, &dbg),
        Err(e) => {
            dbg.log(&format!("handshake failed: {e}"));
            let _ = events.send(WorkerEvent::Ready(Err(e)));
        }
    }

    teardown(&mut master, &socket, &target);
    // Explicitly, not just via `Drop`: a clean exit shouldn't depend on drop order, and the
    // screen is waiting to hear that the socket and its directory are gone.
    socket.cleanup();
    let _ = ack.send(());
}

/// Serve commands until the screen closes. Emits `Ready` first, so the remote pane has a
/// directory to start browsing from.
fn serve(
    socket: &ControlSocket,
    target: &str,
    cmds: &Commands,
    events: &Sender<WorkerEvent>,
    dbg: &DebugLog,
) {
    // Start browsing from the remote working directory (the login/home dir); fall back to root.
    let home = match remote_home(socket, target, cmds, dbg) {
        Ok(home) => home,
        Err(BatchError::Stopped) => return,
        Err(_) => PathBuf::from("/"),
    };
    dbg.log(&format!("master ready; remote home = {}", home.display()));
    let _ = events.send(WorkerEvent::Ready(Ok(home)));

    while let Some(cmd) = cmds.recv() {
        match cmd {
            WorkerCmd::ListRemote(path) => match list_remote(socket, target, &path, cmds, dbg) {
                Ok(listing) => {
                    let _ = events.send(WorkerEvent::Listing {
                        path,
                        entries: listing.entries,
                        truncated: listing.truncated,
                    });
                }
                Err(BatchError::Stopped) => return,
                Err(BatchError::Cancelled) => {
                    // The listing ate the cancel — the screen is blocked waiting to hear that
                    // it took, and any transfer queued behind it has been dropped.
                    dbg.log("listing cancelled");
                    let _ = events.send(WorkerEvent::Cancelled);
                }
                Err(e) => {
                    let what = format!("listing {}", path.display());
                    let _ = events.send(WorkerEvent::Error(e.describe(&what)));
                }
            },
            WorkerCmd::Transfer(job) => match transfer(socket, target, &job, cmds, events, dbg) {
                Ok(()) => {
                    let _ = events.send(WorkerEvent::Done);
                }
                Err(TransferError::Cancelled) => {
                    dbg.log("transfer cancelled");
                    // The screen blocks every other key while a transfer runs, so it needs
                    // to be told the cancel finished or it stays stuck in that state.
                    let _ = events.send(WorkerEvent::Cancelled);
                }
                Err(TransferError::Stopped) => return,
                Err(TransferError::Exists(name)) => {
                    dbg.log(&format!("{name} appeared in the destination — skipped"));
                    let _ = events.send(WorkerEvent::Skipped(name));
                }
                Err(TransferError::Failed(e)) => {
                    let _ = events.send(WorkerEvent::Error(e));
                }
            },
            WorkerCmd::Mkdir(path) => match mkdir_remote(socket, target, &path, cmds, dbg) {
                Ok(()) => {
                    let _ = events.send(WorkerEvent::MkdirDone(Ok(path)));
                }
                Err(BatchError::Stopped) => return,
                Err(BatchError::Cancelled) => {
                    dbg.log("mkdir cancelled");
                    let _ = events.send(WorkerEvent::Cancelled);
                }
                Err(e) => {
                    let what = format!("creating {}", path.display());
                    let _ = events.send(WorkerEvent::MkdirDone(Err(e.describe(&what))));
                }
            },
            // A stray cancel with nothing running: ignore.
            WorkerCmd::Cancel => {}
            WorkerCmd::Shutdown => return,
        }
    }
}

/// Spawn the backgrounded `ssh` ControlMaster, reusing sshelf's askpass wiring so the stored
/// secret authenticates it exactly as a normal connect would.
fn open_master(host: &Host, has_secret: bool, socket: &Path) -> std::io::Result<Child> {
    let mut cmd = Command::new("ssh");
    cmd.args(master_args(host, socket, has_secret));
    ssh::configure_askpass(&mut cmd, host, has_secret, None);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped()); // kept so a failed handshake can explain itself
    cmd.spawn()
}

/// Wait until `ssh -O check` reports the master is up, the master process exits (auth failed),
/// the screen closes (`Shutdown`/dropped channel), or the timeout elapses.
fn handshake(
    socket: &ControlSocket,
    target: &str,
    master: &mut Child,
    cmds: &Commands,
) -> Result<(), String> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        // Let the screen abort a slow connect promptly, so closing it doesn't wait this out.
        if cmds.poll_stop().is_some() {
            let _ = master.kill();
            let _ = master.wait();
            return Err("cancelled".into());
        }
        if master_alive(socket.path(), target) {
            return Ok(());
        }
        match master.try_wait() {
            // The master exited before the socket appeared → authentication/connection failed.
            Ok(Some(_)) => return Err(child_error(master, "connection failed")),
            Ok(None) => {}
            Err(e) => return Err(format!("ssh master error: {e}")),
        }
        if Instant::now() >= deadline {
            let _ = master.kill();
            let _ = master.wait();
            return Err(
                "timed out opening the connection (wrong password, or host unreachable)".into(),
            );
        }
        std::thread::sleep(POLL);
    }
}

/// True if a master is listening on `socket` (`ssh -O check` exits 0). Bounded like every
/// other child here: a `check` that hangs would hang the handshake poll with it.
fn master_alive(socket: &Path, target: &str) -> bool {
    run_bounded(mux_command(master_check_args(socket, target)), MUX_TIMEOUT)
        .is_some_and(|s| s.success())
}

/// `ssh` with a mux control op's argv and every stream closed — neither `-O check` nor
/// `-O exit` has anything to say that the worker reads.
fn mux_command(args: Vec<String>) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Run a child that produces no output, under a deadline. `None` means it never finished on
/// its own and was killed — the point being that the caller returns either way.
///
/// Polled tighter than the sftp batches are: a mux op normally answers in a couple of
/// milliseconds, and the handshake asks one of them how the master is doing every [`POLL`].
fn run_bounded(mut cmd: Command, timeout: Duration) -> Option<ExitStatus> {
    const MUX_POLL: Duration = Duration::from_millis(10);
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => {
                kill_and_reap(&mut child);
                return None;
            }
        }
        if Instant::now() >= deadline {
            kill_and_reap(&mut child);
            return None;
        }
        std::thread::sleep(MUX_POLL);
    }
}

/// What one short-lived `sftp` batch produced.
struct CappedOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    /// Stdout hit its cap, so what is here is only the start of what the server sent. Stderr
    /// has a cap of its own, but hitting it says nothing about the listing, so it goes to the
    /// debug log instead of onto this flag.
    stdout_truncated: bool,
}

/// Why a short-lived `sftp` batch produced nothing.
enum BatchError {
    /// The screen is closing, or its end of the command channel is gone. The command was
    /// consumed here, so the caller has to leave its own loop rather than wait for another.
    Stopped,
    /// The user asked to cancel. The child was killed; the screen is told separately, since a
    /// cancel is not a failure and must not read as one.
    Cancelled,
    /// The child outran its deadline and was killed. The caller names the operation — only it
    /// knows what the batch was doing.
    TimedOut(Duration),
    /// Anything else; the message is safe to show the user.
    Failed(String),
}

impl BatchError {
    /// The message to show, with `what` (`listing /srv/logs`, say) naming the operation.
    fn describe(self, what: &str) -> String {
        match self {
            BatchError::TimedOut(after) => format!("timed out after {}s {what}", after.as_secs()),
            BatchError::Failed(msg) => msg,
            BatchError::Cancelled => format!("cancelled while {what}"),
            BatchError::Stopped => format!("stopped while {what}"),
        }
    }
}

/// A parsed remote listing, plus whether it was cut short — by the output cap or by
/// [`MAX_ENTRIES`] — so the screen can say so instead of showing a partial directory as whole.
struct Listing {
    entries: Vec<RemoteEntry>,
    truncated: bool,
}

/// Run one short-lived `sftp` batch (a listing, a `mkdir`, a `pwd`) over the master and collect
/// its output under a deadline and a size cap.
///
/// A hostile or wedged server must not be able to park a child forever: the screen's teardown
/// waits on this thread, and the terminal comes back only after it. `program` and `timeout` are
/// parameters so the tests can drive a stub on a short fuse without a server — and without
/// touching `PATH`, which is process-global and unsafe to mutate from a test thread.
fn run_sftp_batch(
    program: &Path,
    socket: &Path,
    target: &str,
    batch: &str,
    timeout: Duration,
    cmds: &Commands,
    dbg: &DebugLog,
) -> Result<CappedOutput, BatchError> {
    dbg.log(&format!("sftp> {}", batch.trim_end()));
    let mut child = Command::new(program)
        .args(sftp_batch_args(socket, target))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| BatchError::Failed(format!("could not launch sftp: {e}")))?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(batch.as_bytes());
        // dropped here → EOF → sftp runs the batch, then exits
    }
    // Each stream is drained on its own thread: a pipe that fills up blocks the child, and a
    // blocked child is exactly what the deadline is here to end.
    let stdout = child.stdout.take().map(|s| read_capped(s, STDOUT_CAP));
    let stderr = child.stderr.take().map(|s| read_capped(s, STDERR_CAP));

    let deadline = Instant::now() + timeout;
    let status = loop {
        // A Shutdown or a Cancel has to be seen while the child is still running — otherwise
        // closing the screen would wait out the whole deadline.
        match cmds.poll_stop() {
            Some(Stop::Shutdown) => {
                kill_and_reap(&mut child);
                return Err(BatchError::Stopped);
            }
            Some(Stop::Cancel) => {
                kill_and_reap(&mut child);
                return Err(BatchError::Cancelled);
            }
            None => {}
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                kill_and_reap(&mut child);
                return Err(BatchError::Failed(format!("sftp error: {e}")));
            }
        }
        if Instant::now() >= deadline {
            kill_and_reap(&mut child);
            return Err(BatchError::TimedOut(timeout));
        }
        std::thread::sleep(BATCH_POLL);
    };

    let (stdout, out_capped) = stdout.map(join_capped).unwrap_or_default();
    let (stderr, err_capped) = stderr.map(join_capped).unwrap_or_default();
    if err_capped {
        // Only worth a log line: a chatty server says nothing about whether the listing on
        // stdout is whole, and calling that listing cut short when it isn't would be a lie the
        // send path acts on.
        dbg.log(&format!("  (stderr past {STDERR_CAP} bytes dropped)"));
    }
    Ok(CappedOutput {
        status,
        stdout,
        stderr,
        stdout_truncated: out_capped,
    })
}

/// Drain a child stream on its own thread, keeping at most `cap` bytes and discarding the rest —
/// discarding rather than stopping, because a reader that walks away leaves the child blocked on
/// a full pipe. The flag says whether anything was dropped.
fn read_capped(mut stream: impl Read + Send + 'static, cap: usize) -> JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut kept: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut truncated = false;
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                    truncated |= n > room;
                }
            }
        }
        (kept, truncated)
    })
}

/// Collect one of [`read_capped`]'s threads. A reader has nothing to panic on, but if one did,
/// empty output beats taking the worker down with it.
fn join_capped(handle: JoinHandle<(Vec<u8>, bool)>) -> (String, bool) {
    let (bytes, truncated) = handle.join().unwrap_or_default();
    (String::from_utf8_lossy(&bytes).into_owned(), truncated)
}

/// List a remote directory by running `sftp -b -` over the master and parsing `ls -la`.
/// The `-a` is what makes dotfiles appear: `sftp`'s own `ls` hides them otherwise, while the
/// local pane's `read_dir` never did — the two panes have to show the same thing (D-028).
fn list_remote(
    socket: &ControlSocket,
    target: &str,
    path: &Path,
    cmds: &Commands,
    dbg: &DebugLog,
) -> Result<Listing, BatchError> {
    let batch = format!("ls -la {}\n", shell_quote(&path.to_string_lossy()));
    let out = run_sftp_batch(
        Path::new("sftp"),
        socket.path(),
        target,
        &batch,
        LIST_TIMEOUT,
        cmds,
        dbg,
    )?;
    if !out.status.success() {
        dbg.log(&format!(
            "  ls failed (exit {:?}):\n{}",
            out.status.code(),
            out.stderr.trim_end()
        ));
        return Err(BatchError::Failed(
            tidy_error(&out.stderr).unwrap_or_else(|| format!("could not list {}", path.display())),
        ));
    }
    Ok(parse_listing(&out))
}

/// Turn `ls -la` output into entries: directories first, then case-insensitive by name, which
/// matches the local pane's ordering. Parsing stops at [`MAX_ENTRIES`] — one more line than that
/// is read only to tell a listing that fits from one that was cut.
fn parse_listing(out: &CappedOutput) -> Listing {
    let mut entries: Vec<RemoteEntry> = out
        .stdout
        .lines()
        .filter_map(parse_ls_line)
        .take(MAX_ENTRIES + 1)
        .collect();
    let truncated = out.stdout_truncated || entries.len() > MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Listing { entries, truncated }
}

/// Create one remote directory with `sftp`'s own `mkdir`, which **fails** on an existing name
/// rather than adopting it — the same never-clobber rule transfers follow. Deliberately not
/// `mkdir -p`: the UI creates one directory in the pane's current directory (D-026).
fn mkdir_remote(
    socket: &ControlSocket,
    target: &str,
    path: &Path,
    cmds: &Commands,
    dbg: &DebugLog,
) -> Result<(), BatchError> {
    let batch = format!("mkdir {}\n", shell_quote(&path.to_string_lossy()));
    let out = run_sftp_batch(
        Path::new("sftp"),
        socket.path(),
        target,
        &batch,
        MKDIR_TIMEOUT,
        cmds,
        dbg,
    )?;
    if out.status.success() {
        return Ok(());
    }
    dbg.log(&format!(
        "  mkdir failed (exit {:?}):\n{}",
        out.status.code(),
        out.stderr.trim_end()
    ));
    Err(BatchError::Failed(match tidy_error(&out.stderr) {
        Some(detail) => format!("could not create {}: {detail}", path.display()),
        None => format!("could not create {}", path.display()),
    }))
}

/// Resolve the remote working directory via `sftp`'s `pwd` (`Remote working directory: …`).
fn remote_home(
    socket: &ControlSocket,
    target: &str,
    cmds: &Commands,
    dbg: &DebugLog,
) -> Result<PathBuf, BatchError> {
    let out = run_sftp_batch(
        Path::new("sftp"),
        socket.path(),
        target,
        "pwd\n",
        MKDIR_TIMEOUT,
        cmds,
        dbg,
    )?;
    out.stdout
        .lines()
        .find_map(|l| {
            l.split_once("Remote working directory:")
                .map(|(_, path)| PathBuf::from(path.trim()))
        })
        .ok_or_else(|| BatchError::Failed("sftp reported no working directory".to_string()))
}

/// Why a transfer ended other than success.
enum TransferError {
    /// The UI asked to cancel. Already handled; the screen is told separately.
    Cancelled,
    /// The screen is closing (`Shutdown`, or its channel is gone): the partial file is cleaned
    /// up and the worker leaves its command loop. Nobody is left to tell.
    Stopped,
    /// The destination name appeared between the queue's check and the install, so nothing was
    /// written. Reported as a skip — exactly like the check's own — so a batch send carries on.
    Exists(String),
    /// The transfer failed; the message is safe to show.
    Failed(String),
}

/// The local side of a download: the path `sftp` writes, and — for a single file — the name it
/// is installed under afterwards.
struct LocalDest {
    /// What `sftp get` writes to. Polled for progress, and removed if the transfer is cancelled.
    written: PathBuf,
    /// Set for a single file: `written` is a `.sshelf-part-…` temporary in the destination
    /// directory, installed under this name by [`install_download`] once every byte is there.
    /// `None` for a recursive download — `link()` cannot install a directory, so those write
    /// straight into place and keep only the queue's listing check.
    install_as: Option<PathBuf>,
}

/// Build the `sftp` batch line for a transfer, plus the local paths a download involves. Paths
/// are quoted for sftp's own command parser, which is consistent across OpenSSH versions —
/// unlike `scp`, whose remote-path handling switched to the SFTP protocol in OpenSSH 9 and then
/// takes shell quoting literally, corrupting names with spaces.
fn transfer_batch(job: &TransferJob, name: &str) -> (String, Option<LocalDest>) {
    let flag = if job.recursive { "-r " } else { "" };
    match job.direction {
        Direction::Download => {
            let local = if job.recursive {
                LocalDest {
                    written: job.dest_dir.join(name),
                    install_as: None,
                }
            } else {
                // A single file lands on a private temporary first, so the bytes never go
                // anywhere near a name that appeared since the queue checked the listing.
                LocalDest {
                    written: job.dest_dir.join(format!(".sshelf-part-{}", Ulid::new())),
                    install_as: Some(job.dest_dir.join(name)),
                }
            };
            let line = format!(
                "get {flag}{} {}\n",
                shell_quote(&job.src.to_string_lossy()),
                shell_quote(&local.written.to_string_lossy()),
            );
            (line, Some(local))
        }
        Direction::Upload => {
            let remote_dest = format!("{}/{name}", job.dest_dir.to_string_lossy());
            let line = format!(
                "put {flag}{} {}\n",
                shell_quote(&job.src.to_string_lossy()),
                shell_quote(&remote_dest),
            );
            (line, None)
        }
    }
}

/// Install a finished single-file download under its real name.
///
/// `link()` is the no-replace step a pre-flight listing check can never be: it fails with
/// `EEXIST` when anything is already at `dest` — a symlink included, which it never follows — so
/// a name that appeared in the meantime is stepped over rather than overwritten, and a symlink
/// somebody dropped there cannot redirect the bytes.
///
/// The temporary is removed once it is either installed or refused for a name that is taken.
/// The one case it survives is an install that failed outright: the download is complete and
/// correct, and deleting it would mean fetching it all over again.
fn install_download(tmp: &Path, dest: &Path, name: &str) -> Result<(), TransferError> {
    install_linked(tmp, dest, name, |tmp, dest| std::fs::hard_link(tmp, dest))
}

/// The same with `link` injected, so the tests can drive the paths a filesystem without hard
/// links takes without needing one mounted.
fn install_linked(
    tmp: &Path,
    dest: &Path,
    name: &str,
    link: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), TransferError> {
    match link(tmp, dest) {
        Ok(()) => {
            let _ = std::fs::remove_file(tmp);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(tmp);
            Err(TransferError::Exists(name.to_string()))
        }
        // Not every filesystem has hard links: exFAT and FAT32 — a USB stick, typically — and
        // a fair few SMB and FUSE mounts refuse every `link()` there is. The bytes are all
        // here and correct, so throwing them away would be the worst of the options: check the
        // name is still free and move the temporary onto it instead. The window that reopens
        // is narrower than the one before any of this, when `sftp get` wrote the final name.
        Err(e) if link_unsupported(&e) => {
            if std::fs::symlink_metadata(dest).is_ok() {
                let _ = std::fs::remove_file(tmp);
                return Err(TransferError::Exists(name.to_string()));
            }
            std::fs::rename(tmp, dest).map_err(|e| kept_as_temporary(tmp, dest, name, &e))
        }
        // Anything else: keep the temporary and say where it is. A file the user can rename
        // beats one they have to download again.
        Err(e) => Err(kept_as_temporary(tmp, dest, name, &e)),
    }
}

/// The error for a download that arrived whole but could not be put in place. Names the
/// temporary it is still in — the alternative is deleting somebody's finished transfer.
fn kept_as_temporary(tmp: &Path, dest: &Path, name: &str, e: &std::io::Error) -> TransferError {
    TransferError::Failed(format!(
        "downloaded {name}, but could not put it in place at {}: {e} — it is still here as {}",
        dest.display(),
        tmp.display()
    ))
}

/// Whether `e` says this destination cannot take a hard link at all, as opposed to refusing
/// one because the name is taken. Filesystems that have no links answer one of these for
/// every `link()`, so a download onto them would never install.
#[cfg(unix)]
fn link_unsupported(e: &std::io::Error) -> bool {
    // A list rather than an or-pattern: `ENOTSUP` and `EOPNOTSUPP` are the same number on
    // Linux and two different ones on macOS.
    const NO_LINKS: [i32; 5] = [
        libc::EPERM,
        libc::EOPNOTSUPP,
        libc::ENOTSUP,
        libc::ENOSYS,
        libc::EMLINK,
    ];
    e.raw_os_error()
        .is_some_and(|code| NO_LINKS.contains(&code))
}

#[cfg(not(unix))]
fn link_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
    )
}

/// Run one transfer with `sftp` (`put`/`get` over the master), emitting progress and honoring a
/// mid-flight cancel.
fn transfer(
    socket: &ControlSocket,
    target: &str,
    job: &TransferJob,
    cmds: &Commands,
    events: &Sender<WorkerEvent>,
    dbg: &DebugLog,
) -> Result<(), TransferError> {
    let name = job
        .src
        .file_name()
        .ok_or_else(|| TransferError::Failed("invalid source path".into()))?
        .to_string_lossy()
        .into_owned();
    let (batch, local_dest) = transfer_batch(job, &name);
    dbg.log(&format!("sftp> {}", batch.trim_end()));

    let mut child = Command::new("sftp")
        .args(sftp_batch_args(socket.path(), target))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| TransferError::Failed(format!("could not launch sftp: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(batch.as_bytes());
        // dropped here → EOF → sftp runs the batch line, then exits
    }

    let mut tick = 0u32;
    loop {
        match cmds.poll_stop() {
            Some(Stop::Cancel) => {
                abandon(&mut child, local_dest.as_ref());
                return Err(TransferError::Cancelled);
            }
            Some(Stop::Shutdown) => {
                abandon(&mut child, local_dest.as_ref());
                return Err(TransferError::Stopped);
            }
            None => {}
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let err = drain_stderr(&mut child);
                    dbg.log(&format!(
                        "  transfer failed (exit {:?}):\n{}",
                        status.code(),
                        err.trim_end()
                    ));
                    remove_temporary(local_dest.as_ref());
                    return Err(TransferError::Failed(
                        tidy_error(&err).unwrap_or_else(|| "transfer failed".into()),
                    ));
                }
                if let Some(local) = &local_dest {
                    let done = local_size(&local.written);
                    if let Some(dest) = &local.install_as {
                        install_download(&local.written, dest, &name)?;
                    }
                    // Final 100% tick so the bar lands full.
                    let _ = events.send(WorkerEvent::Progress(Progress {
                        bytes_done: done,
                        bytes_total: job.size_hint.max(done),
                    }));
                }
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => return Err(TransferError::Failed(format!("sftp error: {e}"))),
        }

        if tick.is_multiple_of(PROGRESS_EVERY)
            && let Some(local) = &local_dest
        {
            let _ = events.send(WorkerEvent::Progress(Progress {
                bytes_done: local_size(&local.written),
                bytes_total: job.size_hint,
            }));
        }
        tick = tick.wrapping_add(1);
        std::thread::sleep(POLL);
    }
}

/// Kill the `sftp` child and clear up after it: a download the user walked away from leaves a
/// stub behind — the `.sshelf-part-…` temporary of a single file, or the part of a directory
/// tree that made it across.
fn abandon(child: &mut Child, local: Option<&LocalDest>) {
    kill_and_reap(child);
    if let Some(local) = local {
        let _ = std::fs::remove_file(&local.written)
            .or_else(|_| std::fs::remove_dir_all(&local.written));
    }
}

/// Remove the temporary a failed download was writing. Only ever the temporary: a recursive
/// download writes into the destination itself, and a directory that may not even be ours to
/// begin with is not one to delete after the fact.
fn remove_temporary(local: Option<&LocalDest>) {
    if let Some(local) = local.filter(|l| l.install_as.is_some()) {
        let _ = std::fs::remove_file(&local.written);
    }
}

/// Close the master politely via the mux, then make sure the process is gone. The polite half
/// runs under a deadline too: a master that will not answer `-O exit` must not be able to keep
/// the worker — and the session directory it has yet to remove — hanging around.
fn teardown(master: &mut Child, socket: &ControlSocket, target: &str) {
    let _ = run_bounded(
        mux_command(master_exit_args(socket.path(), target)),
        MUX_TIMEOUT,
    );
    kill_and_reap(master);
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn local_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Take and return a child's captured stderr.
fn drain_stderr(child: &mut Child) -> String {
    let mut buf = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut buf);
    }
    buf
}

/// The most useful line of a child's stderr (ssh/sftp put the real cause last), or `fallback`.
fn child_error(child: &mut Child, fallback: &str) -> String {
    tidy_error(&drain_stderr(child)).unwrap_or_else(|| fallback.to_string())
}

/// The last non-blank line of `raw` (ssh/sftp/scp put the real cause last), if any.
fn tidy_error(raw: &str) -> Option<String> {
    raw.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Parse one `sftp` `ls -la` line into a [`RemoteEntry`], or `None` for prompts, headers, and
/// entries we don't browse (`.`/`..`, sockets/devices). The format (captured from OpenSSH):
/// `mode  links  owner  group  size  month  day  time  PATH` — note `links` is `?` and `PATH`
/// is the full path, so we take its basename. Symlinks show no ` -> target`, just an `l` mode.
fn parse_ls_line(line: &str) -> Option<RemoteEntry> {
    let line = line.trim_end();
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 9 {
        return None;
    }
    let mode = fields[0];
    if mode.len() < 10 {
        return None; // skip "sftp>" echoes, "total N", banners
    }
    let (is_dir, is_symlink) = match mode.as_bytes()[0] {
        b'd' => (true, false),
        b'l' => (false, true),
        b'-' => (false, false),
        _ => return None, // sockets/pipes/devices — not browseable in v1
    };
    let size = fields[4].parse::<u64>().unwrap_or(0);
    // The name is field 8 onward (it may contain spaces); take its basename.
    let raw_name = remainder_from_field(line, 8)?;
    // Skip the self/parent entries — `ls -la` really does list them, and neither pane browses
    // them. Check the raw path's last component, since `Path::file_name("…/.")` yields the
    // parent, not ".".
    if raw_name == "." || raw_name == ".." || raw_name.ends_with("/.") || raw_name.ends_with("/..")
    {
        return None;
    }
    let name = Path::new(raw_name)
        .file_name()?
        .to_string_lossy()
        .into_owned();
    Some(RemoteEntry {
        name,
        is_dir,
        is_symlink,
        size,
    })
}

/// The remainder of `line` from the `n`th (0-based) whitespace-delimited field onward, with
/// internal spaces preserved (remote filenames can contain spaces).
fn remainder_from_field(line: &str, n: usize) -> Option<&str> {
    let mut rest = line.trim_start();
    for _ in 0..n {
        let end = rest.find(char::is_whitespace)?;
        rest = rest[end..].trim_start();
    }
    (!rest.is_empty()).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of this test's own, under the system temp dir.
    fn scratch(what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sshelf-{what}-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The same, but with a short *path*: an AF_UNIX socket has about a hundred bytes to work
    /// with, and macOS's per-user `$TMPDIR` alone eats half of them — which is exactly why the
    /// real code puts its session directories under `$XDG_RUNTIME_DIR` or the data dir.
    fn short_scratch(what: &str) -> PathBuf {
        let base = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = base.join(format!("sshelf-{what}-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write an executable `sh` stub and return its path. The batch tests inject it as the
    /// `sftp` program: mutating `PATH` would be process-global, and tests run on threads.
    fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// A command channel nothing ever sends on. The sender is returned so it stays alive — a
    /// dropped one reads as "the screen went away" and stops the batch immediately.
    fn idle_commands() -> (Sender<WorkerCmd>, Commands) {
        let (tx, rx) = mpsc::channel();
        (tx, Commands::new(rx))
    }

    #[test]
    fn parses_dir_file_and_symlink() {
        let dir = parse_ls_line("drwxr-xr-x    ? me wheel          64 Jun 16 19:09 /tmp/d/subdir")
            .unwrap();
        assert_eq!(dir.name, "subdir");
        assert!(dir.is_dir && !dir.is_symlink && dir.size == 64);

        let file =
            parse_ls_line("-rw-r--r--    ? me wheel        4096 Jun 16 19:09 /tmp/d/readme.txt")
                .unwrap();
        assert_eq!(file.name, "readme.txt");
        assert!(!file.is_dir && !file.is_symlink && file.size == 4096);

        let link =
            parse_ls_line("lrwxr-xr-x    ? me wheel          10 Jun 16 19:09 /tmp/d/link").unwrap();
        assert!(link.is_symlink && !link.is_dir);
        assert_eq!(link.name, "link");
    }

    #[test]
    fn keeps_spaces_in_names_and_takes_basename() {
        let e =
            parse_ls_line("-rw-r--r--    ? me wheel          7 Jun 16 19:09 /tmp/d/my notes.md")
                .unwrap();
        assert_eq!(e.name, "my notes.md");
    }

    /// Issue #15: with `ls -la` the listing carries dot-entries, and a dotfile is an ordinary
    /// entry — only `.` and `..` are dropped.
    #[test]
    fn parses_dotfiles_and_dot_directories() {
        let f = parse_ls_line("-rw-------    ? me wheel        220 Jun 16 19:09 /tmp/d/.bashrc")
            .unwrap();
        assert_eq!(f.name, ".bashrc");
        assert!(!f.is_dir);

        let d = parse_ls_line("drwx------    ? me wheel         96 Jun 16 19:09 /tmp/d/.config")
            .unwrap();
        assert_eq!(d.name, ".config");
        assert!(d.is_dir);
    }

    #[test]
    fn skips_prompts_dots_and_specials() {
        assert!(parse_ls_line("sftp> ls -la /tmp/d").is_none());
        assert!(parse_ls_line("").is_none());
        assert!(parse_ls_line("drwxr-xr-x    ? me wheel  64 Jun 16 19:09 /tmp/d/.").is_none());
        assert!(parse_ls_line("drwxrwxrwt    ? root wheel 2400 Jun 16 19:09 /tmp/d/..").is_none());
        // A socket/pipe is not browseable.
        assert!(parse_ls_line("srwxr-xr-x    ? me wheel  0 Jun 16 19:09 /tmp/d/sock").is_none());
    }

    #[test]
    fn remainder_from_field_preserves_internal_spaces() {
        let line = "a  b   c d   e f g h  the name here";
        assert_eq!(remainder_from_field(line, 8), Some("the name here"));
        assert_eq!(remainder_from_field("only four words here", 8), None);
    }

    #[test]
    fn each_control_socket_gets_its_own_private_session_directory() {
        let parent = short_scratch("mux");
        let a = ControlSocket::new_in(&parent).unwrap();
        let b = ControlSocket::new_in(&parent).unwrap();

        assert_ne!(a.dir, b.dir, "two sessions must not share a directory");
        assert_ne!(a.path(), b.path());
        assert!(a.path().starts_with(&parent));
        assert!(a.path().as_os_str().len() <= MAX_SOCKET_PATH);
        // sshelf makes the directory; `ssh` makes the socket inside it.
        assert!(a.dir.is_dir() && !a.path().exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&a.dir).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o700,
                "nobody else may write the mux directory"
            );
        }

        std::fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn a_socket_that_is_already_there_is_refused_rather_than_adopted() {
        let parent = short_scratch("mux-taken");
        let socket = ControlSocket::new_in(&parent).unwrap();
        assert!(socket.ensure_absent().is_ok(), "a fresh session is clear");

        // `run` asks this before it spawns ssh, so a path somebody else got to first is
        // reported instead of dialled.
        std::fs::write(socket.path(), b"not a socket").unwrap();
        let err = socket.ensure_absent().unwrap_err();
        assert!(err.contains(&socket.path().display().to_string()), "{err}");
        assert!(err.contains("already exists"), "{err}");

        // A symlink counts too — following one is how the socket would be redirected.
        #[cfg(unix)]
        {
            std::fs::remove_file(socket.path()).unwrap();
            std::os::unix::fs::symlink(parent.join("elsewhere.sock"), socket.path()).unwrap();
            assert!(socket.ensure_absent().is_err(), "a symlink is not ours");
        }

        std::fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn dropping_the_control_socket_takes_the_session_directory_with_it() {
        let parent = short_scratch("mux-drop");
        let dir = {
            let socket = ControlSocket::new_in(&parent).unwrap();
            std::fs::write(socket.path(), b"stub").unwrap();
            socket.dir.clone()
        };
        assert!(
            !dir.exists(),
            "the session directory must not outlive the socket"
        );
        std::fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn a_batch_that_outstays_its_deadline_is_killed_and_reaped() {
        let dir = scratch("batch-slow");
        let pidfile = dir.join("pid");
        // `exec`, so the shell *becomes* the sleep: the pid it records is the pid we kill.
        let program = stub(
            &dir,
            "slow-sftp",
            &format!("echo $$ > '{}'\nexec sleep 30", pidfile.display()),
        );
        let (_tx, cmds) = idle_commands();

        let started = Instant::now();
        let err = run_sftp_batch(
            &program,
            Path::new("/nonexistent/m.sock"),
            "deploy@10.0.0.1",
            "ls -la /srv\n",
            // Long enough that a loaded machine still schedules the stub before the deadline —
            // it has a pid to record — and far short of the 30s it would otherwise run for.
            Duration::from_secs(2),
            &cmds,
            &DebugLog(None),
        )
        .err()
        .expect("the stub never exits on its own");
        assert!(matches!(err, BatchError::TimedOut(_)));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the deadline must not wait for the child"
        );
        // The message the pane shows for a real listing that runs out of time.
        assert_eq!(
            BatchError::TimedOut(LIST_TIMEOUT).describe("listing /srv/logs"),
            "timed out after 60s listing /srv/logs"
        );

        let pid = std::fs::read_to_string(&pidfile).unwrap_or_default();
        let pid = pid.trim();
        assert!(!pid.is_empty(), "the stub should have recorded its pid");
        let alive = Command::new("kill")
            .args(["-0", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!alive, "the child must be killed and reaped, not orphaned");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_listing_stops_at_the_entry_cap_and_says_so() {
        let dir = scratch("batch-huge");
        let program = stub(
            &dir,
            "huge-sftp",
            "exec awk 'BEGIN { for (i = 0; i < 100000; i++) \
             printf \"-rw-r--r--    ? me wheel        4 Jun 16 19:09 /srv/f%d\\n\", i }'",
        );
        let (_tx, cmds) = idle_commands();

        let out = run_sftp_batch(
            &program,
            Path::new("/nonexistent/m.sock"),
            "deploy@10.0.0.1",
            "ls -la /srv\n",
            Duration::from_secs(30),
            &cmds,
            &DebugLog(None),
        )
        .unwrap_or_else(|_| panic!("the stub exits on its own"));
        assert!(out.status.success());

        let listing = parse_listing(&out);
        assert_eq!(listing.entries.len(), MAX_ENTRIES);
        assert!(
            listing.truncated,
            "the screen has to be able to say the listing was cut"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_listing_that_fits_is_not_reported_as_cut() {
        let out = CappedOutput {
            status: Command::new("true").status().unwrap(),
            stdout: "-rw-r--r--    ? me wheel 4 Jun 16 19:09 /srv/one\n".to_string(),
            stderr: String::new(),
            stdout_truncated: false,
        };
        let listing = parse_listing(&out);
        assert_eq!(listing.entries.len(), 1);
        assert!(!listing.truncated);
    }

    #[test]
    fn installing_a_download_puts_it_under_its_real_name() {
        let dir = scratch("install-ok");
        let tmp = dir.join(".sshelf-part-test");
        let dest = dir.join("report.pdf");
        std::fs::write(&tmp, b"downloaded").unwrap();

        assert!(
            install_download(&tmp, &dest, "report.pdf").is_ok(),
            "a free name installs"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"downloaded");
        assert!(!tmp.exists(), "the temporary never stays behind");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn installing_a_download_never_replaces_a_name_that_appeared() {
        let dir = scratch("install-exists");
        let tmp = dir.join(".sshelf-part-test");
        let dest = dir.join("report.pdf");
        std::fs::write(&tmp, b"downloaded").unwrap();
        std::fs::write(&dest, b"was already here").unwrap();

        let err = install_download(&tmp, &dest, "report.pdf").unwrap_err();
        assert!(matches!(err, TransferError::Exists(ref n) if n == "report.pdf"));
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"was already here",
            "the file that got there first must survive"
        );
        assert!(!tmp.exists(), "the temporary never stays behind");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn installing_a_download_never_follows_a_symlink_at_the_destination() {
        let dir = scratch("install-symlink");
        let tmp = dir.join(".sshelf-part-test");
        let dest = dir.join("report.pdf");
        let pointed_at = dir.join("someone-elses.txt");
        std::fs::write(&tmp, b"downloaded").unwrap();
        std::fs::write(&pointed_at, b"do not touch").unwrap();
        std::os::unix::fs::symlink(&pointed_at, &dest).unwrap();

        let err = install_download(&tmp, &dest, "report.pdf").unwrap_err();
        assert!(matches!(err, TransferError::Exists(_)));
        assert_eq!(
            std::fs::read(&pointed_at).unwrap(),
            b"do not touch",
            "link() must not write through the symlink"
        );
        assert!(
            std::fs::symlink_metadata(&dest)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink itself is left exactly as it was"
        );
        assert!(!tmp.exists(), "the temporary never stays behind");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn transfer_batch_quotes_paths_for_sftp() {
        // Upload a file whose name has spaces: sftp's parser needs the paths single-quoted
        // (the bug that broke scp — which took the quotes literally).
        let up = TransferJob {
            direction: Direction::Upload,
            src: PathBuf::from("/Users/me/my file.txt"),
            dest_dir: PathBuf::from("/home/r/Downloads"),
            recursive: false,
            size_hint: 0,
        };
        let (line, dest) = transfer_batch(&up, "my file.txt");
        assert_eq!(
            line,
            "put '/Users/me/my file.txt' '/home/r/Downloads/my file.txt'\n"
        );
        assert!(dest.is_none());

        // Recursive download adds -r and writes straight into the destination — `link()` can't
        // install a directory, so those keep the queue's listing check and nothing more.
        let down = TransferJob {
            direction: Direction::Download,
            src: PathBuf::from("/srv/my data"),
            dest_dir: PathBuf::from("/tmp/dl"),
            recursive: true,
            size_hint: 0,
        };
        let (line, dest) = transfer_batch(&down, "my data");
        assert_eq!(line, "get -r '/srv/my data' '/tmp/dl/my data'\n");
        let local = dest.unwrap();
        assert_eq!(local.written, PathBuf::from("/tmp/dl/my data"));
        assert_eq!(local.install_as, None);
    }

    #[test]
    fn a_single_file_download_lands_on_a_temporary_first() {
        let job = TransferJob {
            direction: Direction::Download,
            src: PathBuf::from("/srv/report.pdf"),
            dest_dir: PathBuf::from("/tmp/dl"),
            recursive: false,
            size_hint: 12,
        };
        let (line, dest) = transfer_batch(&job, "report.pdf");
        let local = dest.unwrap();

        assert_eq!(local.written.parent(), Some(Path::new("/tmp/dl")));
        assert!(
            local
                .written
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".sshelf-part-"),
            "{}",
            local.written.display()
        );
        assert_eq!(local.install_as, Some(PathBuf::from("/tmp/dl/report.pdf")));
        // sftp writes the temporary, never the final name.
        assert_eq!(
            line,
            format!("get /srv/report.pdf {}\n", local.written.display())
        );
        assert!(!line.contains("/tmp/dl/report.pdf"));
    }

    /// The cancel the screen sends lands while the destination listing that goes out ahead of
    /// every upload is still running. The batch consumes it, so the batch also has to drop the
    /// transfer it was parked in front of — otherwise the worker runs, one command later, the
    /// very transfer the user just stopped.
    #[test]
    fn a_cancel_takes_the_transfer_parked_behind_the_running_batch_with_it() {
        let dir = scratch("batch-cancel");
        let program = stub(&dir, "slow-sftp", "exec sleep 30");
        let (tx, cmds) = idle_commands();
        // Exactly what `start_next` sends for an upload, followed by the user's Esc.
        tx.send(WorkerCmd::ListRemote(PathBuf::from("/srv")))
            .unwrap();
        tx.send(WorkerCmd::Transfer(TransferJob {
            direction: Direction::Upload,
            src: PathBuf::from("/home/me/report.pdf"),
            dest_dir: PathBuf::from("/srv"),
            recursive: false,
            size_hint: 3,
        }))
        .unwrap();
        tx.send(WorkerCmd::Cancel).unwrap();

        let started = Instant::now();
        let err = run_sftp_batch(
            &program,
            Path::new("/nonexistent/m.sock"),
            "deploy@10.0.0.1",
            "ls -la /srv\n",
            Duration::from_secs(30),
            &cmds,
            &DebugLog(None),
        )
        .err()
        .expect("the cancel ends the batch");
        assert!(
            matches!(err, BatchError::Cancelled),
            "a cancel is not a failure — the screen has to hear it as a cancel"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the cancel must not wait the child out"
        );

        // The listing that was queued behind it is still somebody's pane waiting to load, so
        // it survives. The transfer does not.
        let parked: Vec<&'static str> = cmds
            .deferred
            .borrow()
            .iter()
            .map(|c| match c {
                WorkerCmd::ListRemote(_) => "list",
                WorkerCmd::Transfer(_) => "transfer",
                _ => "other",
            })
            .collect();
        assert_eq!(parked, vec!["list"], "the cancelled transfer must not run");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_mux_control_op_that_never_answers_is_killed_rather_than_waited_on() {
        let mut hangs = Command::new("sleep");
        hangs
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let started = Instant::now();
        assert!(
            run_bounded(hangs, Duration::from_millis(300)).is_none(),
            "teardown cannot wait on a master that will not answer"
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        // One that answers is still reported normally.
        let status = run_bounded(Command::new("true"), Duration::from_secs(5));
        assert!(status.is_some_and(|s| s.success()));
    }

    #[test]
    fn the_session_parent_prefers_the_runtime_directory_and_never_chmods_it() {
        let dir = scratch("parent-runtime");
        let runtime = dir.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();

        let parent = session_parent_in(Some(&runtime), || {
            panic!("the data directory must not be touched when the runtime dir is there")
        })
        .unwrap();
        assert_eq!(parent, runtime.join("sshelf"));
        assert!(parent.is_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&parent).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o700,
                "sshelf makes its own directory private"
            );

            // A directory that is already there is the user's to set: an existing one keeps
            // whatever mode it has, and `$XDG_RUNTIME_DIR` itself is never touched at all.
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
            let again = session_parent_in(Some(&runtime), || panic!("not needed")).unwrap();
            assert_eq!(again, parent);
            let mode = std::fs::metadata(&parent).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o777,
                "an existing directory is not re-chmod-ed"
            );
            let mode = std::fs::metadata(&runtime).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o755,
                "$XDG_RUNTIME_DIR is not ours to change"
            );
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_session_parent_falls_back_to_the_data_directory() {
        let dir = scratch("parent-data");
        let data = dir.join("share").join("sshelf");
        let missing = dir.join("no-such-runtime");

        // A runtime dir that is set but isn't there is no runtime dir…
        let parent = session_parent_in(Some(&missing), || Ok(data.clone())).unwrap();
        assert_eq!(parent, data.join("run"));
        assert!(parent.is_dir());
        // …and neither is one that was never set.
        assert_eq!(
            session_parent_in(None, || Ok(data.clone())).unwrap(),
            parent
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for private in [&data, &parent] {
                let mode = std::fs::metadata(private).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700, "{}", private.display());
            }
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_socket_path_too_long_for_af_unix_is_refused_rather_than_moved_to_tmp() {
        let base = scratch("mux-deep");
        let deep = base.join("d".repeat(60)).join("d".repeat(60));
        std::fs::create_dir_all(&deep).unwrap();

        let err = ControlSocket::new_in(&deep)
            .err()
            .expect("a path this deep cannot hold a unix socket");
        assert!(err.contains("too long"), "{err}");
        assert!(err.contains("XDG_RUNTIME_DIR"), "{err}");
        // And the session directory it had to create to find that out is gone again.
        let left: Vec<PathBuf> = std::fs::read_dir(&deep)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert!(left.is_empty(), "left behind: {left:?}");

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A filesystem with no hard links answers every `link()` this way — inject it rather than
    /// mount a USB stick.
    #[cfg(unix)]
    fn no_links(code: i32) -> impl Fn(&Path, &Path) -> std::io::Result<()> {
        move |_, _| Err(std::io::Error::from_raw_os_error(code))
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_that_cannot_hard_link_still_gets_the_download() {
        let dir = scratch("install-nolink");
        let tmp = dir.join(".sshelf-part-test");
        let dest = dir.join("report.pdf");
        std::fs::write(&tmp, b"downloaded").unwrap();

        // exFAT and FAT32 (a USB stick, typically) and several network mounts have no links at
        // all. The download is finished and correct; it goes into place by rename instead.
        assert!(
            install_linked(&tmp, &dest, "report.pdf", no_links(libc::EOPNOTSUPP)).is_ok(),
            "a finished download must not be thrown away"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"downloaded");
        assert!(!tmp.exists(), "the temporary never stays behind");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_destination_that_cannot_hard_link_still_refuses_a_name_that_is_taken() {
        let dir = scratch("install-nolink-exists");
        let tmp = dir.join(".sshelf-part-test");
        let dest = dir.join("report.pdf");
        std::fs::write(&tmp, b"downloaded").unwrap();
        std::fs::write(&dest, b"was already here").unwrap();

        let err = install_linked(&tmp, &dest, "report.pdf", no_links(libc::EPERM)).unwrap_err();
        assert!(matches!(err, TransferError::Exists(ref n) if n == "report.pdf"));
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"was already here",
            "the fallback is still a no-overwrite install"
        );
        assert!(!tmp.exists(), "the temporary never stays behind");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_download_that_cannot_be_installed_keeps_its_bytes_and_says_where_they_are() {
        let dir = scratch("install-kept");
        let tmp = dir.join(".sshelf-part-test");
        let dest = dir.join("report.pdf");
        std::fs::write(&tmp, b"downloaded").unwrap();

        let err = install_linked(&tmp, &dest, "report.pdf", |_, _| {
            Err(std::io::Error::other("the disk went away"))
        })
        .unwrap_err();
        match err {
            TransferError::Failed(msg) => assert!(
                msg.contains(&tmp.display().to_string()),
                "the user has to be told where the file is: {msg}"
            ),
            _ => panic!("an install that fails outright is a failure, not a skip"),
        }
        assert_eq!(
            std::fs::read(&tmp).unwrap(),
            b"downloaded",
            "a transfer that arrived whole is never deleted for want of a name"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
