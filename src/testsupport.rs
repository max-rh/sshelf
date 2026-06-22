//! Shared test scaffolding: a throwaway, rootless `sshd` on localhost for the `#[ignore]`d e2e
//! tests (transfer + port-forward). Spawning a real `sshd` + `ssh`/`sftp` means these tests only
//! run with `cargo test -- --ignored` on a machine with OpenSSH installed.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use crate::model::{AuthMethod, Host};

static NEXT_PORT: AtomicU16 = AtomicU16::new(47137);

/// A running throwaway `sshd`; killed and cleaned up on drop.
pub(crate) struct Sshd {
    child: Child,
    pub(crate) dir: PathBuf,
    pub(crate) port: u16,
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
pub(crate) fn start_sshd() -> Option<Sshd> {
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
pub(crate) fn host_for(sshd: &Sshd) -> Host {
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
