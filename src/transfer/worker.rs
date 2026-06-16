//! The transfer worker thread and the `ssh` ControlMaster it owns.
//!
//! The TUI event loop is synchronous; this background thread runs the blocking `ssh`/`sftp` so
//! a slow link never freezes the UI. They talk over std channels: the UI sends
//! [`WorkerCmd`], the worker emits [`WorkerEvent`] (drained each tick). The worker owns the
//! master child and its control socket, and tears both down when it stops — on `Shutdown`, a
//! dropped command channel, or a failed handshake.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

/// A handle to the running transfer worker. Dropping it shuts the worker — and the master
/// connection — down, and blocks until that teardown finishes (so the socket is gone even on a
/// panic unwind).
pub struct TransferSession {
    cmd_tx: Sender<WorkerCmd>,
    join: Option<JoinHandle<()>>,
}

impl TransferSession {
    /// Spawn the worker for `host` (`has_secret` decides whether to wire `SSH_ASKPASS`). The
    /// master connection is opened on the worker thread; the first event is a
    /// [`WorkerEvent::Ready`] reporting whether it came up. Returns the handle plus the channel
    /// of events to drain in the UI loop.
    pub fn spawn(host: Host, has_secret: bool) -> std::io::Result<(Self, Receiver<WorkerEvent>)> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name("sshelf-transfer".into())
            .spawn(move || run(host, has_secret, cmd_rx, event_tx))?;
        Ok((
            Self {
                cmd_tx,
                join: Some(join),
            },
            event_rx,
        ))
    }

    /// Queue a command for the worker. A send error means the worker already stopped, in which
    /// case the screen is closing anyway, so it's ignored.
    pub fn send(&self, cmd: WorkerCmd) {
        let _ = self.cmd_tx.send(cmd);
    }
}

impl Drop for TransferSession {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(WorkerCmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// The control socket for the master, kept short (AF_UNIX paths are capped near 104 bytes, and
/// macOS's `$TMPDIR` is far too long) and removed on drop.
struct ControlSocket {
    path: PathBuf,
}

impl ControlSocket {
    fn new() -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!("/tmp/sshelf-mux-{}-{seq}.sock", std::process::id()));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ControlSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The worker thread body: open the master, then service commands until told to stop.
fn run(host: Host, has_secret: bool, cmd_rx: Receiver<WorkerCmd>, events: Sender<WorkerEvent>) {
    let target = target(&host);
    let socket = ControlSocket::new();

    let mut master = match open_master(&host, has_secret, socket.path()) {
        Ok(child) => child,
        Err(e) => {
            let _ = events.send(WorkerEvent::Ready(Err(format!(
                "could not launch ssh: {e}"
            ))));
            return;
        }
    };

    if let Err(e) = handshake(&socket, &target, &mut master, &cmd_rx) {
        let _ = events.send(WorkerEvent::Ready(Err(e)));
        teardown(&mut master, &socket, &target);
        return;
    }
    // Start browsing from the remote working directory (the login/home dir); fall back to root.
    let home = remote_home(&socket, &target).unwrap_or_else(|| PathBuf::from("/"));
    let _ = events.send(WorkerEvent::Ready(Ok(home)));

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            WorkerCmd::ListRemote(path) => match list_remote(&socket, &target, &path) {
                Ok(entries) => {
                    let _ = events.send(WorkerEvent::Listing { path, entries });
                }
                Err(e) => {
                    let _ = events.send(WorkerEvent::Error(e));
                }
            },
            WorkerCmd::Transfer(job) => match transfer(&socket, &target, &job, &cmd_rx, &events) {
                Ok(()) => {
                    let _ = events.send(WorkerEvent::Done);
                }
                Err(TransferError::Cancelled) => {}
                Err(TransferError::Failed(e)) => {
                    let _ = events.send(WorkerEvent::Error(e));
                }
            },
            // A stray cancel with nothing running, or anything else: ignore.
            WorkerCmd::Cancel => {}
            WorkerCmd::Shutdown => break,
        }
    }
    teardown(&mut master, &socket, &target);
}

/// Spawn the backgrounded `ssh` ControlMaster, reusing sshelf's askpass wiring so the stored
/// secret authenticates it exactly as a normal connect would.
fn open_master(host: &Host, has_secret: bool, socket: &Path) -> std::io::Result<Child> {
    let mut cmd = Command::new("ssh");
    cmd.args(master_args(host, socket));
    ssh::configure_askpass(&mut cmd, host, has_secret);
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
    cmd_rx: &Receiver<WorkerCmd>,
) -> Result<(), String> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        // Let the screen abort a slow connect promptly (so Drop's join doesn't block the UI).
        if let Ok(WorkerCmd::Shutdown) | Err(TryRecvError::Disconnected) = cmd_rx.try_recv() {
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

/// True if a master is listening on `socket` (`ssh -O check` exits 0).
fn master_alive(socket: &Path, target: &str) -> bool {
    Command::new("ssh")
        .args(master_check_args(socket, target))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// List a remote directory by running `sftp -b -` over the master and parsing `ls -l`.
fn list_remote(
    socket: &ControlSocket,
    target: &str,
    path: &Path,
) -> Result<Vec<RemoteEntry>, String> {
    let mut child = Command::new("sftp")
        .args(sftp_batch_args(socket.path(), target))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not launch sftp: {e}"))?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "sftp stdin unavailable".to_string())?;
        let line = format!("ls -l {}\n", shell_quote(&path.to_string_lossy()));
        stdin
            .write_all(line.as_bytes())
            .map_err(|e| format!("writing to sftp: {e}"))?;
        // stdin dropped here → EOF → sftp runs the batch and exits.
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("sftp failed: {e}"))?;
    if !out.status.success() {
        return Err(tidy_error(&String::from_utf8_lossy(&out.stderr))
            .unwrap_or_else(|| format!("could not list {}", path.display())));
    }

    let mut entries: Vec<RemoteEntry> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_ls_line)
        .collect();
    // Directories first, then case-insensitive by name — matches the local pane's ordering.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

/// Resolve the remote working directory via `sftp`'s `pwd` (`Remote working directory: …`).
fn remote_home(socket: &ControlSocket, target: &str) -> Option<PathBuf> {
    let mut child = Command::new("sftp")
        .args(sftp_batch_args(socket.path(), target))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    {
        let mut stdin = child.stdin.take()?;
        let _ = stdin.write_all(b"pwd\n");
    }
    let out = child.wait_with_output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        l.split_once("Remote working directory:")
            .map(|(_, path)| PathBuf::from(path.trim()))
    })
}

/// Why a transfer ended other than success.
enum TransferError {
    /// The UI asked to cancel (or its channel closed). Already handled; no event needed.
    Cancelled,
    /// The transfer failed; the message is safe to show.
    Failed(String),
}

/// Build the `sftp` batch line for a transfer, plus the local destination to poll for download
/// progress. Paths are quoted for sftp's own command parser, which is consistent across OpenSSH
/// versions — unlike `scp`, whose remote-path handling switched to the SFTP protocol in OpenSSH
/// 9 and then takes shell quoting literally, corrupting names with spaces.
fn transfer_batch(job: &TransferJob, name: &str) -> (String, Option<PathBuf>) {
    let flag = if job.recursive { "-r " } else { "" };
    match job.direction {
        Direction::Download => {
            let local_dest = job.dest_dir.join(name);
            let line = format!(
                "get {flag}{} {}\n",
                shell_quote(&job.src.to_string_lossy()),
                shell_quote(&local_dest.to_string_lossy()),
            );
            (line, Some(local_dest))
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

/// Run one transfer with `sftp` (`put`/`get` over the master), emitting progress and honoring a
/// mid-flight cancel.
fn transfer(
    socket: &ControlSocket,
    target: &str,
    job: &TransferJob,
    cmd_rx: &Receiver<WorkerCmd>,
    events: &Sender<WorkerEvent>,
) -> Result<(), TransferError> {
    let name = job
        .src
        .file_name()
        .ok_or_else(|| TransferError::Failed("invalid source path".into()))?
        .to_string_lossy()
        .into_owned();
    let (batch, local_dest) = transfer_batch(job, &name);

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
        match cmd_rx.try_recv() {
            Ok(WorkerCmd::Cancel) | Err(TryRecvError::Disconnected) => {
                kill_and_reap(&mut child);
                // A partial download leaves a stub file/dir behind; clean it up.
                if let Some(dest) = &local_dest {
                    let _ = std::fs::remove_file(dest).or_else(|_| std::fs::remove_dir_all(dest));
                }
                return Err(TransferError::Cancelled);
            }
            // Ignore other commands mid-transfer; the UI blocks new actions while one runs.
            Ok(_) => {}
            Err(TryRecvError::Empty) => {}
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    // Final 100% tick so the bar lands full.
                    if let Some(dest) = &local_dest {
                        let done = local_size(dest);
                        let _ = events.send(WorkerEvent::Progress(Progress {
                            bytes_done: done,
                            bytes_total: job.size_hint.max(done),
                        }));
                    }
                    Ok(())
                } else {
                    Err(TransferError::Failed(child_error(
                        &mut child,
                        "transfer failed",
                    )))
                };
            }
            Ok(None) => {}
            Err(e) => return Err(TransferError::Failed(format!("sftp error: {e}"))),
        }

        if tick.is_multiple_of(PROGRESS_EVERY)
            && let Some(dest) = &local_dest
        {
            let _ = events.send(WorkerEvent::Progress(Progress {
                bytes_done: local_size(dest),
                bytes_total: job.size_hint,
            }));
        }
        tick = tick.wrapping_add(1);
        std::thread::sleep(POLL);
    }
}

/// Close the master politely via the mux, then make sure the process is gone.
fn teardown(master: &mut Child, socket: &ControlSocket, target: &str) {
    let _ = Command::new("ssh")
        .args(master_exit_args(socket.path(), target))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    kill_and_reap(master);
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn local_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Read a child's captured stderr and return the most useful line, or `fallback`.
fn child_error(child: &mut Child, fallback: &str) -> String {
    let mut buf = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut buf);
    }
    tidy_error(&buf).unwrap_or_else(|| fallback.to_string())
}

/// The last non-blank line of `raw` (ssh/sftp/scp put the real cause last), if any.
fn tidy_error(raw: &str) -> Option<String> {
    raw.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Parse one `sftp` `ls -l` line into a [`RemoteEntry`], or `None` for prompts, headers, and
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
    // Skip the self/parent entries. `ls -l` (no `-a`) omits them, but be defensive — and check
    // the raw path's last component, since `Path::file_name("…/.")` yields the parent, not ".".
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

    #[test]
    fn skips_prompts_dots_and_specials() {
        assert!(parse_ls_line("sftp> ls -l /tmp/d").is_none());
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
    fn control_socket_path_is_short_and_unique() {
        let a = ControlSocket::new();
        let b = ControlSocket::new();
        assert!(a.path().starts_with("/tmp/"));
        assert!(a.path().to_string_lossy().len() < 104);
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn transfer_batch_quotes_paths_for_sftp() {
        use std::path::PathBuf;
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

        // Recursive download adds -r and reports the local destination to poll for progress.
        let down = TransferJob {
            direction: Direction::Download,
            src: PathBuf::from("/srv/my data"),
            dest_dir: PathBuf::from("/tmp/dl"),
            recursive: true,
            size_hint: 0,
        };
        let (line, dest) = transfer_batch(&down, "my data");
        assert_eq!(line, "get -r '/srv/my data' '/tmp/dl/my data'\n");
        assert_eq!(dest, Some(PathBuf::from("/tmp/dl/my data")));
    }
}
