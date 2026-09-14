//! First connect against a real, rootless `sshd` on localhost (D-035): the prompt, the probe
//! for an encrypted key, the verify, what is kept and what is removed, the 2FA ordering, and
//! the line a refused stored secret prints.
//!
//! `#[ignore]`d like the transfer e2e, so run it with `cargo test -- --ignored` on a machine with
//! OpenSSH. The server offers password auth, but an `sshd` that isn't running as root cannot
//! check a password against the system, so it refuses every one: that is what the password
//! cases here prove. A secret that works is proven with a passphrase-protected key instead,
//! which ssh checks locally before the server ever sees the key.
//!
//! Every connect runs with `HOME` pointed at a scratch directory, a headless vault, no agent
//! (unless a test starts its own), and `-F /dev/null` plus a private `UserKnownHostsFile`, so
//! nothing of the developer's own ssh setup takes part.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_sshelf");
const VAULT_PASS: &str = "first-connect-e2e";

/// A throwaway `sshd` plus an sshelf home around it. Killed and removed on drop.
struct Server {
    child: Child,
    root: PathBuf,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// What a finished `sshelf` run left behind.
struct Run {
    code: Option<i32>,
    stderr: String,
}

fn user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "root".into())
}

fn keygen(path: &Path, passphrase: &str) {
    let ok = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", passphrase, "-f"])
        .arg(path)
        .stdin(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok, "ssh-keygen failed for {}", path.display());
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

const HOSTS: &str = r#"format_version = 1

[[host]]
id = "test-pwhost"
name = "pwbox"
hostname = "127.0.0.1"
user = "{USER}"
port = {PORT}
auth = "password"
extra_args = "{EXTRA}"

[[host]]
id = "test-skiphost"
name = "skipbox"
hostname = "127.0.0.1"
user = "{USER}"
port = {PORT}
auth = "password"
extra_args = "{EXTRA} -o BatchMode=yes"

[[host]]
id = "test-2fahost"
name = "twofa"
hostname = "127.0.0.1"
user = "{USER}"
port = {PORT}
auth = "password"
requires_2fa = true
extra_args = "{EXTRA}"

[[host]]
id = "test-lockedkey"
name = "lockedbox"
hostname = "127.0.0.1"
user = "{USER}"
port = {PORT}
auth = "key"
identity_files = ["{ROOT}/id_locked"]
extra_args = "{EXTRA}"

[[host]]
id = "test-plainkey"
name = "plainbox"
hostname = "127.0.0.1"
user = "{USER}"
port = {PORT}
auth = "key"
identity_files = ["{ROOT}/id_plain"]
extra_args = "{EXTRA}"
"#;

const EXTRA: &str =
    "-F /dev/null -o UserKnownHostsFile={ROOT}/known_hosts -o IdentitiesOnly=yes -o LogLevel=ERROR";

impl Server {
    /// Start the server with `passphrase` on the encrypted key, or `None` when there is no sshd.
    fn start(passphrase: &str) -> Option<Server> {
        if !Path::new("/usr/sbin/sshd").exists() {
            return None;
        }
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        // Short on purpose: OpenSSH cuts a key path in its passphrase prompt at 100 characters.
        let root = std::env::temp_dir().join(format!("sfc-{}-{n}", std::process::id()));
        for dir in ["srv", ".config/sshelf", ".local/share"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        keygen(&root.join("srv/hostkey"), "");
        keygen(&root.join("id_plain"), "");
        keygen(&root.join("id_locked"), passphrase);
        let mut authorized = std::fs::read_to_string(root.join("id_plain.pub")).unwrap();
        authorized.push_str(&std::fs::read_to_string(root.join("id_locked.pub")).unwrap());
        std::fs::write(root.join("srv/authorized_keys"), authorized).unwrap();

        let port = free_port();
        let cfg = root.join("srv/sshd_config");
        std::fs::write(
            &cfg,
            format!(
                "Port {port}\n\
                 ListenAddress 127.0.0.1\n\
                 HostKey {srv}/hostkey\n\
                 PidFile {srv}/pid\n\
                 AuthorizedKeysFile {srv}/authorized_keys\n\
                 PasswordAuthentication yes\n\
                 KbdInteractiveAuthentication no\n\
                 UsePAM no\n\
                 StrictModes no\n",
                srv = root.join("srv").display(),
            ),
        )
        .unwrap();
        let hosts = HOSTS
            .replace("{EXTRA}", EXTRA)
            .replace("{ROOT}", &root.display().to_string())
            .replace("{USER}", &user())
            .replace("{PORT}", &port.to_string());
        std::fs::write(root.join(".config/sshelf/hosts.toml"), hosts).unwrap();

        let child = Command::new("/usr/sbin/sshd")
            .arg("-f")
            .arg(&cfg)
            .args(["-D", "-e"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let server = Server { child, root, port };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Some(server);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    fn endpoint(&self) -> String {
        format!("{}@127.0.0.1:{}", user(), self.port)
    }

    fn key(&self, name: &str) -> String {
        self.root.join(name).display().to_string()
    }

    /// `sshelf`, pointed at this home, a headless vault, and nothing of the developer's.
    fn sshelf(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join(".config"))
            .env("XDG_DATA_HOME", self.root.join(".local/share"))
            .env("SSHELF_VAULT_PASSPHRASE", VAULT_PASS)
            .env_remove("SSHELF_CONFIG")
            .env_remove("SSH_ASKPASS")
            .env_remove("SSH_ASKPASS_REQUIRE")
            .env_remove("SSH_AUTH_SOCK")
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("TMUX");
        c
    }

    /// Run `cmd` with `stdin` as all of its input. Bounded, so a connect that hangs fails the
    /// test rather than the whole run.
    fn run(&self, cmd: &mut Command, stdin: &str) -> Run {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(stdin.as_bytes()).unwrap();
        drop(input);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = rx
            .recv_timeout(Duration::from_secs(90))
            .expect("sshelf did not finish within 90 seconds")
            .unwrap();
        Run {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn connect(&self, host: &str, stdin: &str) -> Run {
        self.run(self.sshelf().arg(host), stdin)
    }

    fn set_password(&self, host: &str, secret: &str) {
        let run = self.run(
            self.sshelf().args(["set-password", host]),
            &format!("{secret}\n"),
        );
        assert_eq!(run.code, Some(0), "{}", run.stderr);
    }

    /// What the askpass helper would hand ssh for `id` right now, i.e. what is stored.
    fn stored(&self, id: &str, kind: &str, prompt: &str, identities: &str) -> Option<String> {
        let out = self
            .sshelf()
            .arg(prompt)
            .env("SSHELF_ASKPASS", "1")
            .env("SSHELF_HOST_ID", id)
            .env("SSHELF_SECRET_KIND", kind)
            .env("SSHELF_IDENTITY_FILES", identities)
            .output()
            .unwrap();
        out.status.success().then(|| {
            String::from_utf8_lossy(&out.stdout)
                .trim_end_matches('\n')
                .to_string()
        })
    }

    fn stored_password(&self, id: &str) -> Option<String> {
        self.stored(id, "password", "Password:", "")
    }

    fn stored_passphrase(&self, id: &str) -> Option<String> {
        let key = self.key("id_locked");
        self.stored(
            id,
            "passphrase",
            &format!("Enter passphrase for key '{key}': "),
            &key,
        )
    }
}

macro_rules! server {
    ($passphrase:expr) => {
        match Server::start($passphrase) {
            Some(server) => server,
            None => {
                eprintln!("skipping e2e: no usable /usr/sbin/sshd on this host");
                return;
            }
        }
    };
}

fn prompt_for(kind_subject: &str) -> String {
    format!("{kind_subject} (saved to your vault once it works; Enter to skip): ")
}

#[test]
#[ignore = "spawns a real sshd + ssh; run with `cargo test -- --ignored`"]
fn a_wrong_password_is_refused_and_nothing_is_kept() {
    let s = server!("unused-a");
    let run = s.connect("pwbox", "not-the-password\n");
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    let prompt = prompt_for(&format!("Password for {}", s.endpoint()));
    assert!(run.stderr.contains(&prompt), "{}", run.stderr);
    assert!(
        run.stderr.contains(&format!(
            "the password was refused by {}; nothing saved",
            s.endpoint()
        )),
        "{}",
        run.stderr
    );
    assert_eq!(s.stored_password("test-pwhost"), None);
    let hosts = std::fs::read_to_string(s.root.join(".config/sshelf/hosts.toml")).unwrap();
    assert!(!hosts.contains("not-the-password"));

    // Nothing was kept, so the next connect asks again.
    let run = s.connect("pwbox", "still-wrong\n");
    assert!(run.stderr.contains(&prompt), "{}", run.stderr);
}

#[test]
#[ignore = "spawns a real sshd + ssh; run with `cargo test -- --ignored`"]
fn enter_skips_and_the_next_connect_asks_again() {
    let s = server!("unused-b");
    // This host's own extra args carry BatchMode=yes, so the skipped connect fails at once
    // instead of asking on a terminal the test doesn't have.
    let run = s.connect("skipbox", "\n");
    assert_eq!(run.code, Some(255), "{}", run.stderr);
    assert!(run.stderr.contains("Password for"), "{}", run.stderr);
    // The prompt itself says "(saved to your vault once it works...)", so look for the outcomes.
    assert!(!run.stderr.contains("saved password"), "{}", run.stderr);
    assert!(!run.stderr.contains("nothing saved"), "{}", run.stderr);
    assert_eq!(s.stored_password("test-skiphost"), None);

    let run = s.connect("skipbox", "\n");
    assert!(run.stderr.contains("Password for"), "{}", run.stderr);
}

#[test]
#[ignore = "spawns a real sshd + ssh; run with `cargo test -- --ignored`"]
fn an_encrypted_key_is_asked_for_checked_and_kept() {
    const PASSPHRASE: &str = "passphrase-for-the-kept-key";
    let s = server!(PASSPHRASE);

    // Watch every process's argv for as long as the connect runs.
    let stop = Arc::new(AtomicBool::new(false));
    let leaked = Arc::new(AtomicBool::new(false));
    let scans = Arc::new(AtomicU32::new(0));
    let watcher = {
        let (stop, leaked, scans) = (stop.clone(), leaked.clone(), scans.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(out) = Command::new("ps").args(["-axww", "-o", "args="]).output() {
                    if String::from_utf8_lossy(&out.stdout).contains(PASSPHRASE) {
                        leaked.store(true, Ordering::Relaxed);
                    }
                    scans.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let run = s.connect("lockedbox", &format!("{PASSPHRASE}\n"));
    stop.store(true, Ordering::Relaxed);
    watcher.join().unwrap();

    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let prompt = prompt_for(&format!("Passphrase for {}", s.key("id_locked")));
    assert!(run.stderr.contains(&prompt), "{}", run.stderr);
    assert!(
        run.stderr.contains("saved passphrase for lockedbox"),
        "{}",
        run.stderr
    );
    assert!(
        scans.load(Ordering::Relaxed) > 0,
        "the argv watcher never ran"
    );
    assert!(
        !leaked.load(Ordering::Relaxed),
        "the passphrase showed up in a process's argv"
    );
    assert_eq!(
        s.stored_passphrase("test-lockedkey").as_deref(),
        Some(PASSPHRASE)
    );

    // The second connect goes straight in.
    let run = s.connect("lockedbox", "");
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(!run.stderr.contains("Passphrase for"), "{}", run.stderr);
}

#[test]
#[ignore = "spawns a real sshd + ssh; run with `cargo test -- --ignored`"]
fn a_wrong_passphrase_is_refused_and_removed() {
    let s = server!("the-real-passphrase");
    let run = s.connect("lockedbox", "not-it\n");
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(
        run.stderr.contains(&format!(
            "the passphrase was refused by {}; nothing saved",
            s.endpoint()
        )),
        "{}",
        run.stderr
    );
    assert_eq!(s.stored_passphrase("test-lockedkey"), None);
}

/// A throwaway `ssh-agent` holding the encrypted key. Killed on drop.
struct Agent {
    child: Child,
    socket: PathBuf,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn agent_with_key(s: &Server, passphrase: &str) -> Agent {
    let socket = s.root.join("agent.sock");
    let child = Command::new("ssh-agent")
        .arg("-D")
        .arg("-a")
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let agent = Agent { child, socket };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !agent.socket.exists() {
        assert!(Instant::now() < deadline, "ssh-agent never made its socket");
        std::thread::sleep(Duration::from_millis(20));
    }
    let askpass = s.root.join("give-passphrase");
    std::fs::write(
        &askpass,
        format!("#!/bin/sh\nprintf '%s\\n' '{passphrase}'\n"),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let added = Command::new("ssh-add")
        .arg(s.root.join("id_locked"))
        .env("SSH_AUTH_SOCK", &agent.socket)
        .env("SSH_ASKPASS", &askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|st| st.success());
    assert!(added, "ssh-add could not load the key");
    agent
}

#[test]
#[ignore = "spawns a real sshd + ssh; run with `cargo test -- --ignored`"]
fn a_key_already_in_the_agent_is_never_asked_about() {
    const PASSPHRASE: &str = "passphrase-for-the-agent-key";
    let s = server!(PASSPHRASE);
    let agent = agent_with_key(&s, PASSPHRASE);

    let run = s.run(
        s.sshelf()
            .arg("lockedbox")
            .env("SSH_AUTH_SOCK", &agent.socket),
        "",
    );
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(!run.stderr.contains("Passphrase for"), "{}", run.stderr);
    assert_eq!(s.stored_passphrase("test-lockedkey"), None);
}

#[test]
#[ignore = "spawns a real sshd + ssh; run with `cargo test -- --ignored`"]
fn an_unencrypted_key_never_prompts() {
    let s = server!("unused-c");
    let run = s.connect("plainbox", "");
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(!run.stderr.contains("Passphrase for"), "{}", run.stderr);
    assert!(!run.stderr.contains("Password for"), "{}", run.stderr);
}

#[test]
#[ignore = "spawns a real sshd + ssh; run with `cargo test -- --ignored`"]
fn a_2fa_host_asks_for_the_secret_and_then_the_code() {
    let s = server!("unused-d");
    let run = s.connect("twofa", "pw-for-2fa\n123456\n");
    let secret = run
        .stderr
        .find("Password for")
        .unwrap_or_else(|| panic!("no secret prompt: {}", run.stderr));
    let saved = run
        .stderr
        .find(
            "saved password for twofa (not checked: this host needs a code; if the login fails, \
             press ^e in the TUI or run sshelf set-password twofa)",
        )
        .unwrap_or_else(|| panic!("no not-checked line: {}", run.stderr));
    let code = run
        .stderr
        .find("Verification code for twofa: ")
        .unwrap_or_else(|| panic!("no code prompt: {}", run.stderr));
    assert!(secret < saved && saved < code, "{}", run.stderr);
    assert_eq!(
        s.stored_password("test-2fahost").as_deref(),
        Some("pw-for-2fa")
    );
}

/// A wrong stored password is named on the terminal, and it is not deleted.
#[test]
#[ignore = "spawns a real sshd + ssh; run with `cargo test -- --ignored`"]
fn a_refused_stored_password_says_so_and_stays() {
    let s = server!("unused-e");
    s.set_password("pwbox", "stale-password");

    let run = s.connect("pwbox", "");
    assert_eq!(run.code, Some(255), "{}", run.stderr);
    assert!(!run.stderr.contains("Password for"), "{}", run.stderr);
    let line = "sshelf: the stored password for test-pwhost was refused; replace it with sshelf \
                set-password or ^e in the TUI";
    assert_eq!(
        run.stderr.matches(line).count(),
        1,
        "the helper's line, once: {}",
        run.stderr
    );
    assert_eq!(
        s.stored_password("test-pwhost").as_deref(),
        Some("stale-password")
    );
}
