//! Transfer engine core: the command lines for the dual-pane file-transfer screen, plus the
//! worker/UI message protocol and progress math.
//!
//! Transport model (validated by the M0 spike — see `docs/decisions.md`): authenticate ONCE by
//! opening a backgrounded `ssh` ControlMaster, which reuses *every* part of sshelf's auth via
//! [`crate::ssh::build_args`] + `SSH_ASKPASS` (keys, agent, ProxyJump, port, and the stored
//! keyring/vault secret). `sftp`/`scp` then ride that master with only `-o ControlPath`, so
//! there is no re-auth and no per-file password prompt. Because the ride commands inherit the
//! connection from the master, they need NONE of `-p`/`-i`/`-J` — which also sidesteps the
//! `ssh -p` vs `sftp`/`scp -P` port-flag difference that would otherwise bite.
#[cfg(test)]
mod e2e;
mod pane;
mod screen;
mod worker;

pub use pane::{Pane, Side};
// Part of `Pane::set_entries`' signature; named directly only by the renderer's tests.
#[cfg_attr(not(test), allow(unused_imports))]
pub use pane::PaneEntry;
pub use screen::{TransferOutcome, TransferScreen};

use std::path::{Path, PathBuf};

use crate::model::Host;

/// Direction of a transfer, named by where the bytes end up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// local → remote
    Upload,
    /// remote → local
    Download,
}

/// The `user@host` destination shared by the master and the `sftp`/`scp` ride commands.
pub fn target(host: &Host) -> String {
    format!("{}@{}", host.effective_user(), host.hostname)
}

/// `ssh` argv that opens a backgrounded ControlMaster for `host`, held by the worker thread.
/// `-N` holds the connection without a remote command; the master is created at `control_path`
/// and [`crate::ssh::build_args`] is reused verbatim so keys/agent/ProxyJump/port and the
/// stored secret (via `SSH_ASKPASS`, wired by the caller) all apply exactly as on connect.
pub fn master_args(host: &Host, control_path: &Path) -> Vec<String> {
    let mut a = vec![
        "-N".to_string(),
        "-o".to_string(),
        "ControlMaster=yes".to_string(),
        "-o".to_string(),
        format!("ControlPath={}", control_path.display()),
    ];
    a.extend(crate::ssh::build_args(host, true));
    a
}

/// `ssh -O check` argv — ask whether the master is alive (worker readiness poll).
pub fn master_check_args(control_path: &Path, target: &str) -> Vec<String> {
    control_op_args("check", control_path, target)
}

/// `ssh -O exit` argv — tell the master to close (teardown).
pub fn master_exit_args(control_path: &Path, target: &str) -> Vec<String> {
    control_op_args("exit", control_path, target)
}

fn control_op_args(op: &str, control_path: &Path, target: &str) -> Vec<String> {
    vec![
        "-O".to_string(),
        op.to_string(),
        "-o".to_string(),
        format!("ControlPath={}", control_path.display()),
        target.to_string(),
    ]
}

/// `sftp -b -` argv that rides the master — used for directory listing (the worker feeds
/// `ls -l …` on stdin). Only `-o ControlPath` + the destination: the master carries auth/port.
pub fn sftp_batch_args(control_path: &Path, target: &str) -> Vec<String> {
    vec![
        "-b".to_string(),
        "-".to_string(),
        "-o".to_string(),
        format!("ControlPath={}", control_path.display()),
        target.to_string(),
    ]
}

/// `scp` argv that rides the master. `src`/`dst` are already-composed endpoints (a local path
/// or a [`remote_spec`]). `-p` preserves mtimes/modes; `-r` recurses into directories. Like
/// the other ride commands it carries only `-o ControlPath` — never `-p`(port)/`-i`/`-J`.
pub fn scp_args(control_path: &Path, recursive: bool, src: &str, dst: &str) -> Vec<String> {
    let mut a = vec![
        "-o".to_string(),
        format!("ControlPath={}", control_path.display()),
        "-p".to_string(), // preserve mtimes/modes (scp's -p, not a port)
    ];
    if recursive {
        a.push("-r".to_string());
    }
    a.push(src.to_string());
    a.push(dst.to_string());
    a
}

/// A remote `scp`/`sftp` endpoint: `user@host:quoted/path`. The path is shell-quoted because
/// `scp` hands the remote path to the remote login shell — quoting keeps spaces, globs, and
/// other metacharacters literal. The `user@host:` prefix is parsed by `scp` locally and must
/// stay unquoted (scp splits on the first colon to separate host from path).
pub fn remote_spec(target: &str, remote_path: &str) -> String {
    format!("{target}:{}", shell_quote(remote_path))
}

/// Quote `s` for a remote shell (or sftp's `ls` parser). Falls back to the raw string only if
/// it contains a NUL, which can't appear in a path anyway.
pub(crate) fn shell_quote(s: &str) -> std::borrow::Cow<'_, str> {
    shlex::try_quote(s).unwrap_or(std::borrow::Cow::Borrowed(s))
}

/// Live transfer progress: bytes moved so far out of the total. `bytes_total` is `0` until the
/// size is known (the UI shows an indeterminate state then).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    pub bytes_done: u64,
    pub bytes_total: u64,
}

impl Progress {
    /// Completion as a whole percentage `0..=100`; `0` when the total isn't known yet.
    pub fn percent(&self) -> u16 {
        if self.bytes_total == 0 {
            return 0;
        }
        let pct = self.bytes_done.saturating_mul(100) / self.bytes_total;
        pct.min(100) as u16
    }
}

/// A remote directory entry, parsed from `sftp`'s `ls -l` output by the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
}

/// One transfer task: move `src` (a file or directory) into the other side's `dest_dir`.
pub struct TransferJob {
    pub direction: Direction,
    /// Absolute path of the item to move, on the source side.
    pub src: PathBuf,
    /// Absolute directory on the destination side to drop it into.
    pub dest_dir: PathBuf,
    /// `src` is a directory (use `scp -r`).
    pub recursive: bool,
    /// Total bytes, when known (a single file's size), for the progress bar; `0` = indeterminate.
    pub size_hint: u64,
}

/// A request from the UI thread to the transfer worker.
pub enum WorkerCmd {
    /// List the remote directory at this absolute path.
    ListRemote(PathBuf),
    /// Run a transfer.
    Transfer(TransferJob),
    /// Cancel the in-flight transfer (if any).
    Cancel,
    /// Tear the master down and stop the worker.
    Shutdown,
}

/// An event from the worker back to the UI, drained on each event-loop tick.
pub enum WorkerEvent {
    /// The master connection finished opening — `Ok(home)` carries the remote working directory
    /// to start browsing from, `Err(msg)` reports why it failed.
    Ready(Result<PathBuf, String>),
    /// A remote-directory listing completed.
    Listing {
        path: PathBuf,
        entries: Vec<RemoteEntry>,
    },
    /// Progress on the in-flight transfer.
    Progress(Progress),
    /// The in-flight transfer completed successfully.
    Done,
    /// A listing or transfer failed; message is safe to show to the user.
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AuthMethod, Host};
    use std::path::Path;

    fn host() -> Host {
        let mut h = Host::new("web", "10.0.0.1");
        h.user = Some("deploy".into());
        h
    }

    fn owned(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn target_is_user_at_host() {
        assert_eq!(target(&host()), "deploy@10.0.0.1");
    }

    #[test]
    fn master_args_open_a_controlmaster_and_reuse_build_args() {
        let a = master_args(&host(), Path::new("/tmp/cm.sock"));
        // Opens a master at our socket, holds the connection (`-N`)…
        assert!(a.contains(&"-N".to_string()));
        assert!(a.windows(2).any(|w| w == ["-o", "ControlMaster=yes"]));
        assert!(a.iter().any(|s| s == "ControlPath=/tmp/cm.sock"));
        // …and reuses build_args: the endpoint is last and StrictHostKeyChecking is carried.
        assert_eq!(a.last().unwrap(), "deploy@10.0.0.1");
        assert!(a.iter().any(|s| s == "StrictHostKeyChecking=accept-new"));
    }

    #[test]
    fn master_carries_jump_hosts_so_transfers_ride_them() {
        // A ProxyJump target works: the master opens through the jump (key/agent), and
        // sftp/scp ride that one connection — no per-command jump setup.
        let mut h = host();
        h.jump_hosts = vec!["bastion".into()];
        let a = master_args(&h, Path::new("/tmp/cm"));
        let j = a.iter().position(|s| s == "-J").expect("jump flag present");
        assert_eq!(a[j + 1], "bastion");
    }

    #[test]
    fn password_host_master_has_no_identity_flag() {
        // Password hosts carry no `-i`; the secret is supplied to the master via SSH_ASKPASS
        // (wired by the caller through ssh::configure_askpass), exactly as for connect.
        let mut h = host();
        h.auth = AuthMethod::Password;
        let a = master_args(&h, Path::new("/tmp/cm"));
        assert!(!a.iter().any(|s| s == "-i"));
        assert_eq!(a.last().unwrap(), "deploy@10.0.0.1");
    }

    #[test]
    fn ride_commands_carry_only_controlpath() {
        // The master already holds port/identity/jump, so the ride commands must NOT repeat
        // them — and a port flag here would be wrong anyway (sftp/scp use `-P`, not ssh's `-p`).
        let cp = Path::new("/tmp/cm.sock");
        assert_eq!(
            sftp_batch_args(cp, "deploy@10.0.0.1"),
            owned(&[
                "-b",
                "-",
                "-o",
                "ControlPath=/tmp/cm.sock",
                "deploy@10.0.0.1"
            ])
        );
        let scp = scp_args(cp, false, "a.txt", "deploy@10.0.0.1:b.txt");
        assert!(!scp.iter().any(|s| s == "-i" || s == "-J"));
        assert!(scp.iter().any(|s| s == "ControlPath=/tmp/cm.sock"));
        assert_eq!(scp[scp.len() - 2], "a.txt");
        assert_eq!(scp[scp.len() - 1], "deploy@10.0.0.1:b.txt");
    }

    #[test]
    fn scp_recursive_adds_dash_r() {
        assert!(scp_args(Path::new("/tmp/cm"), true, "dir", "t:dir").contains(&"-r".to_string()));
        assert!(!scp_args(Path::new("/tmp/cm"), false, "f", "t:f").contains(&"-r".to_string()));
    }

    #[test]
    fn control_ops_target_the_socket() {
        assert_eq!(
            master_check_args(Path::new("/tmp/cm"), "deploy@h"),
            owned(&["-O", "check", "-o", "ControlPath=/tmp/cm", "deploy@h"])
        );
        assert_eq!(
            master_exit_args(Path::new("/tmp/cm"), "deploy@h")[1],
            "exit"
        );
    }

    #[test]
    fn remote_spec_quotes_metacharacters_in_the_path() {
        let s = remote_spec("deploy@h", "/srv/my data/app.log");
        assert!(s.starts_with("deploy@h:"));
        assert!(s.contains("'/srv/my data/app.log'"));
        // A plain path needs no quoting.
        assert_eq!(remote_spec("deploy@h", "/srv/app"), "deploy@h:/srv/app");
    }

    #[test]
    fn progress_percent_clamps_and_handles_unknown_total() {
        assert_eq!(Progress::default().percent(), 0);
        assert_eq!(
            Progress {
                bytes_done: 50,
                bytes_total: 200
            }
            .percent(),
            25
        );
        // Never exceeds 100, even if a stat overshoots.
        assert_eq!(
            Progress {
                bytes_done: 999,
                bytes_total: 100
            }
            .percent(),
            100
        );
    }
}
