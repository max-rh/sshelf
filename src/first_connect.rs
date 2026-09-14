//! Saving the secret on first connect (D-035).
//!
//! A connect to a host with nothing stored asks for the password or key passphrase on the
//! restored terminal, proves it against the server with a throwaway `ssh ... exit`, and keeps it
//! only once it worked. This module holds the decisions, which are pure and unit-tested, and the
//! bounded child processes they need (`ssh-keygen -y`, the probe, the verify). The terminal
//! prompt and the `exec()` stay with the rest of the connect path in `main.rs`.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::model::{AuthMethod, Host};
use crate::ssh;

/// How long `ssh-keygen -y` gets to say whether a key is encrypted. It only reads a local file.
const KEYGEN_DEADLINE: Duration = Duration::from_secs(5);
/// How long the probe or the verify gets, end to end. `ConnectTimeout` is half of it.
const SSH_DEADLINE: Duration = Duration::from_secs(30);

/// What a connect should do about a missing secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Today's path: nothing to ask for.
    Connect,
    /// A password host with nothing stored: ask for it.
    AskPassword,
    /// A key that needs a passphrase: probe with `BatchMode` first, and ask only when the
    /// server refuses, so a key already in the agent is never asked about.
    ProbeKey,
    /// tmux mode: the question has to be asked on this terminal, so the connection can't go to
    /// a new window.
    StepAside,
}

/// The first-connect trigger, over everything it depends on.
///
/// `stored`: a secret is already in the keyring or vault. `encrypted_key`: one of a key host's
/// identity files needs a passphrase. `terminal_jump`: the jump chain takes
/// [`ssh::JumpPlan::Terminal`], where no helper is wired, so there is nothing to save into and
/// ssh asks on the terminal as it always has. `tmux`: the decision is being made for the TUI's
/// tmux mode, where the probe can't run (it happens after teardown, never inside the event loop).
pub fn decide(
    auth: AuthMethod,
    stored: bool,
    encrypted_key: bool,
    terminal_jump: bool,
    tmux: bool,
) -> Decision {
    if stored || terminal_jump {
        return Decision::Connect;
    }
    let wanted = match auth {
        AuthMethod::Password => Decision::AskPassword,
        AuthMethod::Key if encrypted_key => Decision::ProbeKey,
        AuthMethod::Key | AuthMethod::Agent => return Decision::Connect,
    };
    if tmux { Decision::StepAside } else { wanted }
}

/// [`decide`] for a real host, doing only the IO the answer needs: the jump plan is pure,
/// `ssh-keygen` runs only for a key host, and `stored` is asked only when the rest could still
/// lead to a prompt, so an agent host never touches the keyring here.
///
/// Returns the decision and, for a key host, the first identity file that needs a passphrase,
/// as typed in `hosts.toml`.
pub fn assess(
    host: &Host,
    stored: impl FnOnce() -> bool,
    tmux: bool,
) -> (Decision, Option<String>) {
    let terminal_jump = matches!(ssh::jump_plan(host, true), ssh::JumpPlan::Terminal);
    let key = match host.auth {
        AuthMethod::Key if !terminal_jump => first_encrypted_key(host),
        _ => None,
    };
    let could_ask = !terminal_jump && (host.auth == AuthMethod::Password || key.is_some());
    let stored = could_ask && stored();
    let decision = decide(host.auth, stored, key.is_some(), terminal_jump, tmux);
    (decision, key)
}

/// What `ssh-keygen -y -P ''` says about one key file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCheck {
    /// It loaded with an empty passphrase.
    Unencrypted,
    /// It needs a passphrase.
    Encrypted,
    /// Missing, unreadable, not a key, or too slow: don't prompt, and let ssh report it.
    Unknown,
}

/// Read `ssh-keygen`'s exit status and stderr. `None` is a run that never finished.
pub fn classify_keygen(status: Option<i32>, stderr: &str) -> KeyCheck {
    match status {
        Some(0) => KeyCheck::Unencrypted,
        Some(_) if stderr.to_lowercase().contains("passphrase") => KeyCheck::Encrypted,
        _ => KeyCheck::Unknown,
    }
}

fn check_key(path: &str) -> KeyCheck {
    let mut cmd = Command::new("ssh-keygen");
    // `-P ''` answers the one question it would ask, so it never reaches for the terminal.
    cmd.args(["-y", "-P", "", "-f"])
        .arg(ssh::expand_tilde(path))
        .env_remove("SSH_ASKPASS")
        .env_remove("SSH_ASKPASS_REQUIRE");
    match run_bounded(cmd, KEYGEN_DEADLINE) {
        Some(run) => classify_keygen(run.code, &run.stderr),
        None => KeyCheck::Unknown,
    }
}

/// OpenSSH prints at most this many bytes of a key's path in its passphrase prompt
/// (`Enter passphrase for key '%.100s': `).
const PROMPT_PATH_MAX: usize = 100;

/// Whether ssh's passphrase prompt for `key` names it in full. The helper matches that path
/// exactly against the host's identity files, so a longer one can never be answered, and asking
/// for its passphrase would only end with a correct one reported as refused.
fn prompt_names_key(key: &str) -> bool {
    ssh::expand_tilde(key).len() <= PROMPT_PATH_MAX
}

/// The first of a key host's identity files that needs a passphrase the helper could supply, as
/// typed.
pub fn first_encrypted_key(host: &Host) -> Option<String> {
    if host.auth != AuthMethod::Key {
        return None;
    }
    host.identity_files
        .iter()
        .filter(|key| prompt_names_key(key))
        .find(|key| check_key(key) == KeyCheck::Encrypted)
        .cloned()
}

/// What the `BatchMode` probe of an encrypted-key host found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// ssh got in without asking: the agent or an unencrypted sibling key already works.
    Satisfied,
    /// The server refused the key: this is what the passphrase is for.
    Ask,
    /// Anything else (unreachable, timed out): connect normally and let ssh show the real error.
    Unreachable,
}

/// Read the probe's exit status. OpenSSH exits 255 for its own failures and otherwise passes the
/// remote command's status through.
///
/// A 2FA key host can't pass a `BatchMode` probe at all, since the code step needs an answer. A
/// key the server *accepted* still ends in `Permission denied (keyboard-interactive)` there, so
/// for those hosts only a denial that still offers `publickey` counts as the key being refused.
pub fn read_probe(status: Option<i32>, stderr: &str, requires_2fa: bool) -> Probe {
    match status {
        Some(255) => {
            let denial = stderr.lines().find(|l| l.contains("Permission denied"));
            match denial {
                Some(line) if !requires_2fa || line.contains("publickey") => Probe::Ask,
                _ => Probe::Unreachable,
            }
        }
        Some(_) => Probe::Satisfied,
        None => Probe::Unreachable,
    }
}

/// Run the probe for `host`. A run that never finishes counts as unreachable.
pub fn probe(host: &Host) -> Probe {
    match run_bounded(ssh::probe_command(host), SSH_DEADLINE) {
        Some(run) => read_probe(run.code, &run.stderr, host.requires_2fa),
        None => Probe::Unreachable,
    }
}

/// How the verify of a freshly stored secret went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Any exit status but 255: ssh got as far as running `exit` on the server.
    Worked,
    /// 255 and `Permission denied`: the secret is wrong.
    Refused,
    /// 255 for any other reason, a signal, or the deadline. Carries ssh's stderr.
    Failed(String),
}

/// The exit-status rule. OpenSSH defines it: 255 is ssh's own failure, anything else is the
/// remote `exit`, so any status other than 255 means the secret worked.
pub fn read_verify(status: Option<i32>, stderr: &str) -> Verdict {
    match status {
        Some(255) if stderr.contains("Permission denied") => Verdict::Refused,
        Some(255) | None => Verdict::Failed(stderr.to_string()),
        Some(_) => Verdict::Worked,
    }
}

/// Prove the stored secret for `host` with one throwaway `ssh ... exit`.
pub fn verify(host: &Host) -> Verdict {
    match run_bounded(ssh::verify_command(host), SSH_DEADLINE) {
        Some(run) => read_verify(run.code, &run.stderr),
        None => Verdict::Failed(format!(
            "no answer within {} seconds",
            SSH_DEADLINE.as_secs()
        )),
    }
}

/// A finished child: its exit code (`None` for a signal) and everything it wrote to stderr.
struct Run {
    code: Option<i32>,
    stderr: String,
}

/// Run `cmd` with stdin and stdout closed and stderr captured, for at most `deadline`. `None`
/// when it could not start or ran out of time, in which case it has been killed.
fn run_bounded(mut cmd: Command, deadline: Duration) -> Option<Run> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let end = Instant::now() + deadline;
    let mut child = cmd.spawn().ok()?;
    // Read on a thread so a chatty child can't fill the pipe and stall.
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    // A `ProxyCommand` grandchild can hold the pipe open after ssh itself has exited, so the
    // read gets a short grace period rather than a blocking join.
    let grace = end
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(500));
    let stderr = rx
        .recv_timeout(grace)
        .map(|buf| String::from_utf8_lossy(&buf).into_owned())
        .unwrap_or_default();
    Some(Run {
        code: status.code(),
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use AuthMethod::{Agent, Key, Password};

    /// The trigger matrix: every combination of the five inputs has one right answer.
    #[test]
    fn the_trigger_matrix() {
        for auth in [Password, Key, Agent] {
            for stored in [false, true] {
                for encrypted in [false, true] {
                    for terminal_jump in [false, true] {
                        for tmux in [false, true] {
                            let got = decide(auth, stored, encrypted, terminal_jump, tmux);
                            let expected = if stored || terminal_jump {
                                Decision::Connect
                            } else {
                                match (auth, encrypted, tmux) {
                                    (Password, _, false) => Decision::AskPassword,
                                    (Key, true, false) => Decision::ProbeKey,
                                    (Password, _, true) | (Key, true, true) => Decision::StepAside,
                                    _ => Decision::Connect,
                                }
                            };
                            assert_eq!(
                                got, expected,
                                "auth={auth:?} stored={stored} encrypted={encrypted} \
                                 terminal_jump={terminal_jump} tmux={tmux}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn an_agent_host_never_reads_the_store() {
        let h = Host::new("vpn", "10.0.0.3");
        let (decision, key) = assess(&h, || panic!("the store must not be read"), false);
        assert_eq!((decision, key), (Decision::Connect, None));
    }

    #[test]
    fn a_multi_hop_password_host_never_reads_the_store() {
        let mut h = Host::new("deep", "10.0.0.9");
        h.auth = Password;
        h.jump_hosts = vec!["b1".into(), "b2".into()];
        let (decision, _) = assess(&h, || panic!("the store must not be read"), false);
        assert_eq!(decision, Decision::Connect);
    }

    #[test]
    fn a_password_host_asks_only_while_nothing_is_stored() {
        let mut h = Host::new("legacy", "10.0.0.2");
        h.auth = Password;
        assert_eq!(assess(&h, || false, false).0, Decision::AskPassword);
        assert_eq!(assess(&h, || true, false).0, Decision::Connect);
        assert_eq!(assess(&h, || false, true).0, Decision::StepAside);
        // One jump host is constrained with a ProxyCommand, so the helper is still wired.
        h.jump_hosts = vec!["bastion".into()];
        assert_eq!(assess(&h, || false, false).0, Decision::AskPassword);
    }

    #[test]
    fn keygen_output_is_read_by_status_and_wording() {
        assert_eq!(classify_keygen(Some(0), ""), KeyCheck::Unencrypted);
        assert_eq!(
            classify_keygen(
                Some(255),
                "Load key \"/k\": incorrect passphrase supplied to decrypt private key\n"
            ),
            KeyCheck::Encrypted
        );
        assert_eq!(
            classify_keygen(Some(255), "Load key \"/k\": No such file or directory\n"),
            KeyCheck::Unknown
        );
        assert_eq!(classify_keygen(None, "passphrase"), KeyCheck::Unknown);
    }

    #[test]
    fn a_missing_key_file_is_not_encrypted() {
        let mut h = Host::new("web", "10.0.0.1");
        h.auth = Key;
        h.identity_files = vec![format!("/nonexistent/sshelf-{}", ulid::Ulid::new())];
        assert_eq!(first_encrypted_key(&h), None);
    }

    /// OpenSSH 10.3 was seen printing `'/private/tmp/.../scratch'` for a 120-byte key path.
    #[test]
    fn a_key_path_openssh_would_cut_short_is_never_asked_about() {
        assert!(
            prompt_names_key(&format!("/{}", "k".repeat(99))),
            "100 bytes"
        );
        assert!(
            !prompt_names_key(&format!("/{}", "k".repeat(100))),
            "101 bytes"
        );
    }

    #[test]
    fn the_probe_asks_only_when_the_key_itself_was_refused() {
        let denied = "user@h: Permission denied (publickey).\r\n";
        assert_eq!(read_probe(Some(0), "", false), Probe::Satisfied);
        assert_eq!(read_probe(Some(1), "", false), Probe::Satisfied);
        assert_eq!(read_probe(Some(255), denied, false), Probe::Ask);
        assert_eq!(
            read_probe(
                Some(255),
                "ssh: connect to host h port 22: Connection refused\r\n",
                false
            ),
            Probe::Unreachable
        );
        assert_eq!(read_probe(None, denied, false), Probe::Unreachable);

        // 2FA key host: the key passed and only the code step is left.
        let key_ok = "user@h: Permission denied (keyboard-interactive).\r\n";
        assert_eq!(read_probe(Some(255), key_ok, true), Probe::Unreachable);
        let key_refused = "user@h: Permission denied (publickey,keyboard-interactive).\r\n";
        assert_eq!(read_probe(Some(255), key_refused, true), Probe::Ask);
    }

    /// 255 is ssh's own failure; anything else is the remote `exit`.
    #[test]
    fn the_verify_is_read_off_the_exit_status() {
        assert_eq!(read_verify(Some(0), ""), Verdict::Worked);
        assert_eq!(read_verify(Some(1), "junk"), Verdict::Worked);
        assert_eq!(read_verify(Some(254), ""), Verdict::Worked);
        assert_eq!(
            read_verify(Some(255), "u@h: Permission denied (password).\r\n"),
            Verdict::Refused
        );
        assert_eq!(
            read_verify(Some(255), "ssh: Could not resolve hostname nope\r\n"),
            Verdict::Failed("ssh: Could not resolve hostname nope\r\n".into())
        );
        assert!(matches!(read_verify(None, ""), Verdict::Failed(_)));
    }

    #[test]
    fn a_bounded_run_is_killed_at_its_deadline() {
        let started = Instant::now();
        let mut sleeper = Command::new("sleep");
        sleeper.arg("30");
        assert!(run_bounded(sleeper, Duration::from_millis(300)).is_none());
        assert!(started.elapsed() < Duration::from_secs(5));

        let mut sh = Command::new("sh");
        sh.args(["-c", "echo oops >&2; exit 3"]);
        let run = run_bounded(sh, Duration::from_secs(5)).unwrap();
        assert_eq!(run.code, Some(3));
        assert_eq!(run.stderr, "oops\n");
    }
}
