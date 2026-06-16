//! End-to-end transfer tests against a throwaway, rootless `sshd` on localhost.
//!
//! These spawn a real `sshd` plus `ssh`/`sftp`, so they are `#[ignore]`d — run them with
//! `cargo test -- --ignored` on a machine with OpenSSH installed. They drive the worker exactly
//! as the transfer screen does (open the master, list a remote directory, copy files both ways,
//! recursively, with a filename containing spaces), and confirm the ControlMaster + `sftp`
//! transport works against a real server. The unit tests cover the pure pieces (argv builders,
//! `ls -l` parsing, progress math); this is the integration layer the M0 spike proved by hand.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::model::{AuthMethod, Host};

use super::worker::TransferSession;
use super::{Direction, TransferJob, WorkerCmd, WorkerEvent};

static NEXT_PORT: AtomicU16 = AtomicU16::new(47137);

/// A running throwaway `sshd`; killed and cleaned up on drop.
struct Sshd {
    child: Child,
    dir: PathBuf,
    port: u16,
    key: PathBuf,
    known_hosts: PathBuf,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn sftp_server() -> Option<&'static str> {
    [
        "/usr/libexec/sftp-server",     // macOS
        "/usr/lib/openssh/sftp-server", // Debian/Ubuntu
        "/usr/lib/ssh/sftp-server",     // Arch
    ]
    .into_iter()
    .find(|p| Path::new(p).exists())
}

fn keygen(path: &Path) {
    let ok = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", ""])
        .arg("-f")
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "ssh-keygen failed");
}

/// Start a key-auth `sshd` on a free localhost port, or `None` if the host lacks the binaries.
fn start_sshd() -> Option<Sshd> {
    let sftp = sftp_server()?;
    if !Path::new("/usr/sbin/sshd").exists() {
        return None;
    }
    let dir = std::env::temp_dir().join(format!("sshelf-e2e-{}", ulid::Ulid::new()));
    std::fs::create_dir_all(&dir).ok()?;
    let hostkey = dir.join("hostkey");
    let key = dir.join("id");
    keygen(&hostkey);
    keygen(&key);
    let authorized = dir.join("authorized_keys");
    std::fs::copy(dir.join("id.pub"), &authorized).ok()?;

    let port = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
    let cfg = dir.join("sshd_config");
    std::fs::write(
        &cfg,
        format!(
            "Port {port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey {hostkey}\n\
             PidFile {pid}\n\
             AuthorizedKeysFile {authorized}\n\
             PasswordAuthentication no\n\
             UsePAM no\n\
             StrictModes no\n\
             Subsystem sftp {sftp}\n",
            hostkey = hostkey.display(),
            pid = dir.join("pid").display(),
            authorized = authorized.display(),
        ),
    )
    .ok()?;

    let child = Command::new("/usr/sbin/sshd")
        .arg("-f")
        .arg(&cfg)
        .args(["-D", "-e"])
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let sshd = Sshd {
        child,
        dir: dir.clone(),
        port,
        key,
        known_hosts: dir.join("known_hosts"),
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Some(sshd);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None // `sshd` dropped here → killed + cleaned up
}

/// A `Host` pointing at the throwaway server (key auth; test-only ssh options via `extra_args`).
fn host_for(sshd: &Sshd) -> Host {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "root".into());
    let mut h = Host::new("e2e", "127.0.0.1");
    h.user = Some(user);
    h.port = Some(sshd.port);
    h.auth = AuthMethod::Key;
    h.identity_files = vec![sshd.key.to_string_lossy().into_owned()];
    h.extra_args = Some(format!(
        "-o UserKnownHostsFile={} -o IdentitiesOnly=yes",
        sshd.known_hosts.display()
    ));
    h
}

/// Block (up to 15s) for the next event matching `pred`, returning it.
fn recv_until(rx: &Receiver<WorkerEvent>, pred: impl Fn(&WorkerEvent) -> bool) -> WorkerEvent {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("timed out waiting for a worker event");
        let event = rx
            .recv_timeout(remaining)
            .expect("worker channel closed or timed out");
        if pred(&event) {
            return event;
        }
    }
}

#[test]
#[ignore = "spawns a real sshd + ssh/sftp/scp; run with `cargo test -- --ignored`"]
fn lists_and_transfers_both_directions() {
    let Some(sshd) = start_sshd() else {
        eprintln!("skipping e2e: no usable sshd / sftp-server on this host");
        return;
    };

    // Remote tree (remote == localhost): a dir with files (incl. a name with spaces) + a subdir.
    let remote = sshd.dir.join("remote");
    std::fs::create_dir_all(remote.join("sub")).unwrap();
    std::fs::write(remote.join("hello.txt"), b"hello from remote").unwrap();
    std::fs::write(remote.join("a name with spaces.txt"), b"spaced").unwrap();
    std::fs::write(remote.join("sub/inner.txt"), b"deep").unwrap();

    // Enable the diagnostic log and confirm it captures the commands (the `--transfer-log` /
    // SSHELF_TRANSFER_LOG feature). SAFETY: this #[ignore]d test owns its process.
    let log_path = sshd.dir.join("transfer.log");
    unsafe { std::env::set_var(super::LOG_ENV, &log_path) };

    let (session, events) = TransferSession::spawn(host_for(&sshd), false).unwrap();

    // The master opens and reports a working directory.
    let home = match recv_until(&events, |e| matches!(e, WorkerEvent::Ready(_))) {
        WorkerEvent::Ready(Ok(home)) => home,
        WorkerEvent::Ready(Err(e)) => panic!("master failed to open: {e}"),
        _ => unreachable!(),
    };
    // Confirms `sftp pwd` parsing — a parse failure would fall back to "/".
    assert!(home.is_absolute());
    assert_ne!(
        home,
        Path::new("/"),
        "remote home fell back to / (pwd parse failed?)"
    );

    // List the remote directory.
    session.send(WorkerCmd::ListRemote(remote.clone()));
    let WorkerEvent::Listing { entries, .. } =
        recv_until(&events, |e| matches!(e, WorkerEvent::Listing { .. }))
    else {
        unreachable!()
    };
    assert!(entries.iter().any(|e| e.name == "hello.txt" && !e.is_dir));
    assert!(entries.iter().any(|e| e.name == "sub" && e.is_dir));

    // Download a single file.
    let dl = sshd.dir.join("download");
    std::fs::create_dir_all(&dl).unwrap();
    session.send(WorkerCmd::Transfer(TransferJob {
        direction: Direction::Download,
        src: remote.join("hello.txt"),
        dest_dir: dl.clone(),
        recursive: false,
        size_hint: 0,
    }));
    expect_done(&events, "download");
    assert_eq!(
        std::fs::read(dl.join("hello.txt")).unwrap(),
        b"hello from remote"
    );

    // Regression: a filename with spaces (scp's quoting corrupted these; sftp get/put is fine).
    session.send(WorkerCmd::Transfer(TransferJob {
        direction: Direction::Download,
        src: remote.join("a name with spaces.txt"),
        dest_dir: dl.clone(),
        recursive: false,
        size_hint: 0,
    }));
    expect_done(&events, "spaced download");
    assert_eq!(
        std::fs::read(dl.join("a name with spaces.txt")).unwrap(),
        b"spaced"
    );

    // Upload a single file.
    let up = sshd.dir.join("upload.txt");
    std::fs::write(&up, b"hello from local").unwrap();
    let remote_dst = sshd.dir.join("remote-dst");
    std::fs::create_dir_all(&remote_dst).unwrap();
    session.send(WorkerCmd::Transfer(TransferJob {
        direction: Direction::Upload,
        src: up,
        dest_dir: remote_dst.clone(),
        recursive: false,
        size_hint: 0,
    }));
    expect_done(&events, "upload");
    assert_eq!(
        std::fs::read(remote_dst.join("upload.txt")).unwrap(),
        b"hello from local"
    );

    // Regression: upload a filename with spaces.
    let up_spaced = sshd.dir.join("local with spaces.txt");
    std::fs::write(&up_spaced, b"up spaced").unwrap();
    session.send(WorkerCmd::Transfer(TransferJob {
        direction: Direction::Upload,
        src: up_spaced,
        dest_dir: remote_dst.clone(),
        recursive: false,
        size_hint: 0,
    }));
    expect_done(&events, "spaced upload");
    assert_eq!(
        std::fs::read(remote_dst.join("local with spaces.txt")).unwrap(),
        b"up spaced"
    );

    // Recursive directory download (sftp get -r mirrors the source dir into the dest path).
    let dl2 = sshd.dir.join("download2");
    std::fs::create_dir_all(&dl2).unwrap();
    session.send(WorkerCmd::Transfer(TransferJob {
        direction: Direction::Download,
        src: remote.clone(),
        dest_dir: dl2.clone(),
        recursive: true,
        size_hint: 0,
    }));
    expect_done(&events, "recursive download");
    assert_eq!(
        std::fs::read(dl2.join("remote/sub/inner.txt")).unwrap(),
        b"deep"
    );

    // The diagnostic log recorded the master + the sftp commands (and no secrets to leak).
    let logged = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        logged.contains("$ ssh "),
        "log should record the master command"
    );
    assert!(
        logged.contains("sftp> get "),
        "log should record get commands"
    );
    assert!(
        logged.contains("sftp> put "),
        "log should record put commands"
    );
    unsafe { std::env::remove_var(super::LOG_ENV) };

    drop(session); // tears the master + control socket down
}

fn expect_done(rx: &Receiver<WorkerEvent>, what: &str) {
    match recv_until(rx, |e| {
        matches!(e, WorkerEvent::Done | WorkerEvent::Error(_))
    }) {
        WorkerEvent::Done => {}
        WorkerEvent::Error(e) => panic!("{what} failed: {e}"),
        _ => unreachable!(),
    }
}
