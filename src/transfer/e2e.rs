//! End-to-end transfer tests against a throwaway, rootless `sshd` on localhost.
//!
//! These spawn a real `sshd` plus `ssh`/`sftp`, so they are `#[ignore]`d — run them with
//! `cargo test -- --ignored` on a machine with OpenSSH installed. They drive the worker exactly
//! as the transfer screen does (open the master, list a remote directory, copy files both ways,
//! recursively, with a filename containing spaces), and confirm the ControlMaster + `sftp`
//! transport works against a real server. The unit tests cover the pure pieces (argv builders,
//! `ls -l` parsing, progress math); this is the integration layer the M0 spike proved by hand.

use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::testsupport::{host_for, start_sshd};

use super::worker::TransferSession;
use super::{Direction, Side, TransferJob, TransferScreen, WorkerCmd, WorkerEvent};

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

/// Drive the screen's event loop (the app polls it the same way) until `until` holds.
fn pump(screen: &mut TransferScreen, what: &str, until: impl Fn(&TransferScreen) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        screen.drain_events();
        if until(screen) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "timed out waiting for {what} (status: {:?})",
        screen.status()
    );
}

fn k(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn type_name(screen: &mut TransferScreen, name: &str) {
    for c in name.chars() {
        screen.on_key(k(KeyCode::Char(c)));
    }
}

/// Multi-select sends and the `F7` new-directory input, driven through the real screen against
/// the throwaway `sshd` — the pieces the unit tests can only exercise with a stub worker.
#[test]
#[ignore = "spawns a real sshd + ssh/sftp; run with `cargo test -- --ignored`"]
fn marks_queue_transfers_and_mkdir_works_on_both_sides() {
    let Some(sshd) = start_sshd() else {
        eprintln!("skipping e2e: no usable sshd / sftp-server on this host");
        return;
    };

    // Local: three files and a directory. Remote: only `dup.txt`, which must survive untouched.
    let local = sshd.dir.join("local");
    std::fs::create_dir_all(local.join("sub")).unwrap();
    std::fs::write(local.join("one.txt"), b"one").unwrap();
    std::fs::write(local.join("two.txt"), b"two").unwrap();
    std::fs::write(local.join("dup.txt"), b"local version").unwrap();
    std::fs::write(local.join("sub/inner.txt"), b"deep").unwrap();
    let remote = sshd.dir.join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::write(remote.join("dup.txt"), b"remote version").unwrap();

    let mut screen = TransferScreen::open(&host_for(&sshd), false, local.clone()).unwrap();
    pump(&mut screen, "the master to come up", |s| !s.is_connecting());
    screen.goto(Side::Remote, remote.clone());
    pump(&mut screen, "the remote listing", |s| {
        !s.remote_pane().rows().is_empty()
    });

    // ---- upload: mark everything, send, and watch the duplicate get stepped over ----
    screen.on_key(ctrl(KeyCode::Char('a'))); // marks the four local entries, not `..`
    assert_eq!(screen.local_pane().marked_count(), 4);
    screen.on_key(ctrl(KeyCode::Char('s')));
    pump(&mut screen, "the upload queue to drain", |s| {
        s.active().is_none() && s.status().is_some_and(|t| t.starts_with("sent "))
    });

    let status = screen.status().unwrap().to_string();
    assert!(status.starts_with("sent 3 of 4"), "{status}");
    assert!(status.contains("dup.txt (already there)"), "{status}");
    assert_eq!(std::fs::read(remote.join("one.txt")).unwrap(), b"one");
    assert_eq!(std::fs::read(remote.join("two.txt")).unwrap(), b"two");
    assert_eq!(
        std::fs::read(remote.join("sub/inner.txt")).unwrap(),
        b"deep"
    );
    assert_eq!(
        std::fs::read(remote.join("dup.txt")).unwrap(),
        b"remote version",
        "a skip must never overwrite the destination"
    );

    // The queue refreshes the destination when it drains; wait for that listing to land.
    pump(&mut screen, "the refreshed remote listing", |s| {
        s.remote_pane()
            .rows()
            .iter()
            .any(|(e, ..)| e.name == "one.txt")
    });

    // ---- download: the other direction, into an empty local directory ----
    let down = sshd.dir.join("down");
    std::fs::create_dir_all(&down).unwrap();
    screen.goto(Side::Local, down.clone());
    assert_eq!(screen.local_pane().cwd, down);
    screen.on_key(k(KeyCode::Tab)); // focus the remote pane
    screen.on_key(ctrl(KeyCode::Char('a')));
    assert_eq!(
        screen.remote_pane().marked_count(),
        4,
        "dup.txt plus the three that were just uploaded"
    );
    screen.on_key(ctrl(KeyCode::Char('s')));
    pump(&mut screen, "the download queue to drain", |s| {
        s.active().is_none() && s.status().is_some_and(|t| t.starts_with("sent "))
    });
    assert_eq!(std::fs::read(down.join("one.txt")).unwrap(), b"one");
    assert_eq!(std::fs::read(down.join("two.txt")).unwrap(), b"two");
    assert_eq!(std::fs::read(down.join("sub/inner.txt")).unwrap(), b"deep");

    // ---- mkdir on the remote side ----
    screen.on_key(k(KeyCode::F(7)));
    type_name(&mut screen, "releases");
    screen.on_key(k(KeyCode::Enter));
    pump(&mut screen, "the remote mkdir", |s| {
        s.status() == Some("created releases/")
    });
    assert!(remote.join("releases").is_dir());
    pump(&mut screen, "the refreshed remote listing", |s| {
        s.remote_pane()
            .rows()
            .iter()
            .any(|(e, ..)| e.name == "releases")
    });
    assert_eq!(
        screen.remote_pane().selected_entry().unwrap().name,
        "releases",
        "the new directory should be under the cursor"
    );

    // …and a second time with the same name must error, never adopt the existing one.
    screen.on_key(k(KeyCode::F(7)));
    type_name(&mut screen, "releases");
    screen.on_key(k(KeyCode::Enter));
    let status = screen.status().unwrap();
    assert!(status.contains("already exists here"), "{status}");
    assert!(
        screen.mkdir_input().is_some(),
        "a rejected name leaves the input open so it can be fixed"
    );

    // ---- mkdir on the local side ----
    screen.on_key(k(KeyCode::Esc)); // abandon the rejected name
    assert!(screen.mkdir_input().is_none());
    screen.on_key(k(KeyCode::Tab));
    screen.on_key(k(KeyCode::F(7)));
    type_name(&mut screen, "archive");
    screen.on_key(k(KeyCode::Enter));
    assert_eq!(screen.status(), Some("created archive/"));
    assert!(down.join("archive").is_dir());
    assert_eq!(
        screen.local_pane().selected_entry().unwrap().name,
        "archive"
    );

    drop(screen); // tears the master + control socket down
}

/// A remote `mkdir` must fail loudly rather than adopt an existing directory — the same
/// never-clobber rule transfers follow.
#[test]
#[ignore = "spawns a real sshd + ssh/sftp; run with `cargo test -- --ignored`"]
fn remote_mkdir_refuses_an_existing_name() {
    let Some(sshd) = start_sshd() else {
        eprintln!("skipping e2e: no usable sshd / sftp-server on this host");
        return;
    };
    let remote = sshd.dir.join("remote");
    std::fs::create_dir_all(remote.join("taken")).unwrap();
    std::fs::write(remote.join("taken/keep.txt"), b"keep me").unwrap();

    let (session, events) = TransferSession::spawn(host_for(&sshd), false).unwrap();
    recv_until(&events, |e| matches!(e, WorkerEvent::Ready(_)));

    session.send(WorkerCmd::Mkdir(remote.join("fresh")));
    match recv_until(&events, |e| matches!(e, WorkerEvent::MkdirDone(_))) {
        WorkerEvent::MkdirDone(Ok(path)) => assert_eq!(path, remote.join("fresh")),
        WorkerEvent::MkdirDone(Err(e)) => panic!("mkdir should have succeeded: {e}"),
        _ => unreachable!(),
    }
    assert!(remote.join("fresh").is_dir());

    session.send(WorkerCmd::Mkdir(remote.join("taken")));
    match recv_until(&events, |e| matches!(e, WorkerEvent::MkdirDone(_))) {
        WorkerEvent::MkdirDone(Ok(_)) => panic!("an existing name must not be adopted"),
        WorkerEvent::MkdirDone(Err(e)) => {
            // The message names the path and carries sftp's own reason.
            assert!(e.contains("taken"), "{e}");
            assert!(e.starts_with("could not create"), "{e}");
        }
        _ => unreachable!(),
    }
    // The existing directory is untouched.
    assert_eq!(
        std::fs::read(remote.join("taken/keep.txt")).unwrap(),
        b"keep me"
    );

    drop(session);
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
