//! Headless `SSH_ASKPASS` helper mode.
//!
//! `ssh` invokes us as `sshelf "<prompt>"` (with `SSHELF_ASKPASS=1` in the environment). Because
//! a connect that auto-supplies a stored secret runs with `SSH_ASKPASS_REQUIRE=force`, *every*
//! interactive prompt is routed here (proven by the M0 spikes — see docs/ssh-command.md), and a
//! prompt we decline is NOT retried on the terminal — it simply fails.
//!
//! Prompt text is **server-controlled**: OpenSSH hands keyboard-interactive prompts straight
//! through, so a hostile endpoint can ask `Password:` and a hostile jump hop can ask anything at
//! all. Shape-matching alone is therefore not enough — `Password:` is a perfectly valid shape.
//! The connect that wired us also told us *which* secret we hold (`SSHELF_SECRET_KIND`) and,
//! for a key host, which key files are actually in play (`SSHELF_IDENTITY_FILES`), so we answer:
//!
//!   - a **login-password** prompt → the stored secret, but only for a `password` host;
//!   - OpenSSH's own **key-passphrase** prompt, naming one of this host's identity files → the
//!     stored secret, but only for a `key` host;
//!   - any **other** prompt → the one-time 2FA code in `SSHELF_2FA_CODE`, if one was queued for
//!     this connection (the user entered it just before connecting);
//!   - anything else, including a secret-shaped prompt of the wrong kind → decline (exit
//!     non-zero). A missing or unreadable kind declines everything: fail closed.
//!
//! A stored secret that is wrong would otherwise be handed over again on every retry, and the
//! failure would read like the server refusing you. So each connect carries an id
//! (`SSHELF_CONNECT_ID`), and the helper leaves a marker the first time it answers a secret prompt
//! in that connect. The same prompt coming back proves the answer was refused: the helper says so
//! once on stderr and declines. It never deletes the secret, because a server can have its own
//! reasons to ask twice (D-035).
//!
//! See `docs/security.md` and decisions D-029 and D-035.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::display;
use crate::paths::Paths;
use crate::secrets;

const HOST_ID_ENV: &str = "SSHELF_HOST_ID";
/// Env var carrying a one-time verification code the user entered for this connection.
pub(crate) const CODE_ENV: &str = "SSHELF_2FA_CODE";
/// Env var naming which secret the stored value for this host is (see [`SecretKind`]).
pub(crate) const KIND_ENV: &str = "SSHELF_SECRET_KIND";
/// Env var carrying the host's identity files, `:`-separated and already `~`-expanded. Only a
/// passphrase prompt naming one of these is answered.
pub(crate) const IDENTITY_ENV: &str = "SSHELF_IDENTITY_FILES";
/// Env var carrying an id minted fresh for each wired `ssh` command. Not a secret.
pub(crate) const CONNECT_ID_ENV: &str = "SSHELF_CONNECT_ID";
/// The end of the line printed when a stored secret is refused. The transfer and forward screens
/// look for it in ssh's stderr (`ssh::classify_auth_error`).
pub(crate) const REFUSED_HINT: &str =
    "was refused; replace it with sshelf set-password or ^e in the TUI";

/// Markers are `askpass-<connect id>` inside sshelf's private runtime directory.
const MARKER_PREFIX: &str = "askpass-";
/// A marker older than this belongs to a connect that is long over.
const MARKER_MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// OpenSSH's local key-passphrase prompt, `Enter passphrase for key '<path>': `.
const PASSPHRASE_PREFIX: &str = "enter passphrase for key '";
const PASSPHRASE_SUFFIX: &str = "':";

/// Which secret the connect that wired this helper is holding — i.e. the host's auth method.
/// The helper answers only prompts that match its own kind, so a server that asks the *other*
/// shape gets nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SecretKind {
    /// `auth = "password"`: the stored value is a login password.
    Password,
    /// `auth = "key"`: the stored value is a private-key passphrase.
    Passphrase,
    /// `auth = "agent"`: there is no stored secret at all, so only a queued verification code
    /// can ever be answered.
    Agent,
}

impl SecretKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SecretKind::Password => "password",
            SecretKind::Passphrase => "passphrase",
            SecretKind::Agent => "agent",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "password" => Some(SecretKind::Password),
            "passphrase" => Some(SecretKind::Passphrase),
            "agent" => Some(SecretKind::Agent),
            _ => None,
        }
    }
}

/// What a given prompt should be answered with (decided without IO, so it's unit-testable).
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// The stored login password / key passphrase.
    Secret,
    /// The queued one-time 2FA code.
    Code,
    /// Nothing — decline.
    Decline,
}

/// Decide how to answer `prompt`, given the kind of secret this connection holds, the identity
/// files it is using, and whether a one-time code is queued.
///
/// The two secret shapes are checked first and each is scoped to its own kind, so a
/// server-controlled `Password:` never reaches a key passphrase and a forged passphrase prompt
/// never reaches a login password. Neither is ever answered with the verification code. Any
/// prompt that is not secret-shaped is the 2FA step, when a code was queued. Host-key prompts
/// never reach here in practice — connect passes `StrictHostKeyChecking=accept-new`.
fn classify(prompt: &str, kind: Option<SecretKind>, identities: &[&str], has_code: bool) -> Answer {
    // No kind (or one we don't recognise) means we can't tell what we're holding. Fail closed.
    let Some(kind) = kind else {
        return Answer::Decline;
    };
    if is_password_prompt(prompt) {
        return match kind {
            SecretKind::Password => Answer::Secret,
            _ => Answer::Decline,
        };
    }
    if let Some(key) = passphrase_key_path(prompt) {
        // The path OpenSSH names has to be one we actually passed with `-i`; anything else is
        // someone else's prompt.
        let ours = identities.contains(&key);
        return match (kind, ours) {
            (SecretKind::Passphrase, true) => Answer::Secret,
            _ => Answer::Decline,
        };
    }
    if has_code {
        Answer::Code
    } else {
        Answer::Decline
    }
}

/// Run askpass mode for the given prompt; returns the process exit code.
pub fn run(prompt: &str) -> i32 {
    let code = std::env::var(CODE_ENV).ok().filter(|c| !c.is_empty());
    let kind = std::env::var(KIND_ENV)
        .ok()
        .and_then(|k| SecretKind::parse(&k));
    let identity_env = std::env::var(IDENTITY_ENV).unwrap_or_default();
    let identities: Vec<&str> = identity_env.split(':').filter(|s| !s.is_empty()).collect();
    match classify(prompt, kind, &identities, code.is_some()) {
        Answer::Secret => {
            if refused_in_this_connect(prompt, kind) {
                return 1;
            }
            supply_secret()
        }
        Answer::Code => {
            let code = zeroize::Zeroizing::new(code.unwrap_or_default());
            // ssh reads one line and strips the trailing newline.
            println!("{}", code.as_str());
            0
        }
        Answer::Decline => 1,
    }
}

/// True when this secret prompt already had an answer earlier in the same connect, which means
/// that answer was refused. Says so on stderr the first time. Fails open: with no connect id (a
/// tmux window, which never gets one) or no runtime directory, the prompt is answered as before.
fn refused_in_this_connect(prompt: &str, kind: Option<SecretKind>) -> bool {
    let Some(connect_id) = std::env::var(CONNECT_ID_ENV).ok().filter(|v| !v.is_empty()) else {
        return false;
    };
    let Ok(dir) = crate::paths::runtime_dir() else {
        return false;
    };
    match mark(&dir, &connect_id, prompt) {
        Marker::Repeat { warn } => {
            if warn {
                let host = std::env::var(HOST_ID_ENV).unwrap_or_default();
                let kind = kind.map_or("secret", SecretKind::as_str);
                eprintln!(
                    "sshelf: the stored {kind} for {} {REFUSED_HINT}",
                    display::sanitize(&host)
                );
            }
            true
        }
        Marker::First | Marker::Other | Marker::Unavailable => false,
    }
}

/// What the marker for one connect says about the prompt in hand.
#[derive(Debug, PartialEq, Eq)]
enum Marker {
    /// No secret prompt was answered in this connect yet; one now is.
    First,
    /// A secret prompt was answered, but a different one (a second key file's passphrase).
    Other,
    /// The same prompt again. `warn` is true only the first time, since a password prompt comes
    /// back once for every attempt ssh has left.
    Repeat { warn: bool },
    /// No usable marker (bad id, no directory): answer as if there were none.
    Unavailable,
}

/// Record the prompt about to be answered in `dir/askpass-<connect_id>`, or read back the one
/// already there. The file is created exclusively at mode 0600 and holds the prompt it answered,
/// plus a second line once the refusal has been reported.
fn mark(dir: &Path, connect_id: &str, prompt: &str) -> Marker {
    if connect_id.is_empty() || !connect_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Marker::Unavailable;
    }
    let path = dir.join(format!("{MARKER_PREFIX}{connect_id}"));
    let prompt = prompt.trim().replace(['\n', '\r'], " ");
    let mut create = std::fs::OpenOptions::new();
    create.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        create.mode(0o600);
    }
    match create.open(&path) {
        Ok(mut file) => {
            let _ = writeln!(file, "{prompt}");
            Marker::First
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let Ok(text) = std::fs::read_to_string(&path) else {
                return Marker::Unavailable;
            };
            let mut lines = text.lines();
            if lines.next() != Some(prompt.as_str()) {
                return Marker::Other;
            }
            let warned = lines.next().is_some();
            if !warned {
                let _ = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .and_then(|mut f| writeln!(f, "refused"));
            }
            Marker::Repeat { warn: !warned }
        }
        Err(_) => Marker::Unavailable,
    }
}

/// Remove `askpass-*` markers in sshelf's runtime directory that are older than ten minutes. Run
/// before wiring a new connect; never creates the directory.
pub(crate) fn sweep_stale_markers() {
    if let Some(dir) = crate::paths::existing_runtime_dir() {
        remove_stale_markers(&dir, SystemTime::now());
    }
}

fn remove_stale_markers(dir: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(MARKER_PREFIX)
        {
            continue;
        }
        // `DirEntry::metadata` does not follow a symlink, so only a real file is ever removed.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let stale = meta.is_file()
            && meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age > MARKER_MAX_AGE);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Look up and print the stored secret for `SSHELF_HOST_ID`; exit code per success.
fn supply_secret() -> i32 {
    let Ok(id) = std::env::var(HOST_ID_ENV) else {
        return 1;
    };
    if id.is_empty() {
        return 1;
    }
    let Ok(paths) = Paths::resolve() else {
        return 1;
    };
    match secrets::get_password(&paths.vault_file(), &id) {
        Ok(Some(pw)) => {
            let pw = zeroize::Zeroizing::new(pw);
            println!("{}", pw.as_str());
            0
        }
        _ => 1,
    }
}

/// True if the prompt has the shape of a **login password** request: classic password auth
/// (`user@host's password:`) and PAM (`Password:`) both end that way.
///
/// The shape check alone rejects phishing text like "Type your password to continue:", but a
/// server can ask a well-shaped `Password:` over keyboard-interactive whenever it likes, which
/// is why [`classify`] also requires the host to be a password host.
fn is_password_prompt(prompt: &str) -> bool {
    prompt.trim().to_lowercase().ends_with("password:")
}

/// The key path inside OpenSSH's own passphrase prompt, `Enter passphrase for key '<path>': `,
/// or `None` if the prompt is not exactly that shape.
///
/// The prefix and suffix are ASCII, so they're matched case-insensitively on the raw bytes: the
/// path between them is returned untouched, because comparing it against the host's identity
/// files has to be byte-exact on a case-sensitive filesystem.
fn passphrase_key_path(prompt: &str) -> Option<&str> {
    let trimmed = prompt.trim();
    let bytes = trimmed.as_bytes();
    let (head, tail) = (PASSPHRASE_PREFIX.as_bytes(), PASSPHRASE_SUFFIX.as_bytes());
    if bytes.len() < head.len() + tail.len() {
        return None;
    }
    if !bytes[..head.len()].eq_ignore_ascii_case(head) || !trimmed.ends_with(PASSPHRASE_SUFFIX) {
        return None;
    }
    Some(&trimmed[head.len()..trimmed.len() - tail.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "/home/u/.ssh/id_ed25519";

    fn keys() -> Vec<&'static str> {
        vec![KEY]
    }

    #[test]
    fn recognizes_the_two_openssh_secret_shapes() {
        assert!(is_password_prompt("tester@host's password: "));
        assert!(is_password_prompt("Password:"));
        assert!(!is_password_prompt("Verification code: "));
        assert!(!is_password_prompt("Type your password to continue:"));

        assert_eq!(
            passphrase_key_path("Enter passphrase for key '/home/u/.ssh/id_ed25519': "),
            Some(KEY)
        );
        // The path keeps its case; only the fixed text around it is matched case-insensitively.
        assert_eq!(
            passphrase_key_path("enter passphrase for key '/Home/U/Key':"),
            Some("/Home/U/Key")
        );
        assert_eq!(passphrase_key_path("Enter passphrase for key: "), None);
        assert_eq!(passphrase_key_path("Verification code: "), None);
    }

    /// The whole point of D-029: every combination of (kind, prompt) has one right answer, and
    /// a secret-shaped prompt of the wrong kind is never answered — not even with the 2FA code.
    #[test]
    fn classify_matrix() {
        use SecretKind::{Agent, Passphrase, Password};

        let ours = format!("Enter passphrase for key '{KEY}': ");
        let theirs = "Enter passphrase for key '/tmp/evil': ";

        for has_code in [false, true] {
            let code = |a: Answer| if has_code { a } else { Answer::Decline };

            // A password host answers password prompts and nothing else that is secret-shaped.
            let k = Some(Password);
            assert_eq!(
                classify("tester@host's password: ", k, &keys(), has_code),
                Answer::Secret
            );
            assert_eq!(classify("Password:", k, &keys(), has_code), Answer::Secret);
            assert_eq!(
                classify(&ours, k, &keys(), has_code),
                Answer::Decline,
                "a password host must not answer a passphrase prompt"
            );

            // A key host answers only its own key's passphrase prompt. A server asking
            // `Password:` over keyboard-interactive gets nothing — this is finding H-02.
            let k = Some(Passphrase);
            assert_eq!(classify(&ours, k, &keys(), has_code), Answer::Secret);
            assert_eq!(
                classify(theirs, k, &keys(), has_code),
                Answer::Decline,
                "a passphrase prompt naming someone else's key must be declined"
            );
            assert_eq!(
                classify("Password:", k, &keys(), has_code),
                Answer::Decline,
                "a key host must never hand its passphrase to a password prompt"
            );
            assert_eq!(
                classify("tester@host's password: ", k, &keys(), has_code),
                Answer::Decline
            );

            // An agent host holds no secret at all; only the code is ever available.
            let k = Some(Agent);
            assert_eq!(classify("Password:", k, &keys(), has_code), Answer::Decline);
            assert_eq!(classify(&ours, k, &keys(), has_code), Answer::Decline);
            assert_eq!(
                classify("Verification code: ", k, &keys(), has_code),
                code(Answer::Code)
            );

            // Everything that is not secret-shaped is the verification step.
            for k in [Some(Password), Some(Passphrase)] {
                assert_eq!(
                    classify("Verification code: ", k, &keys(), has_code),
                    code(Answer::Code)
                );
                assert_eq!(
                    classify("Type your password to continue:", k, &keys(), has_code),
                    code(Answer::Code)
                );
                assert_eq!(
                    classify("One-time password (OATH): ", k, &keys(), has_code),
                    code(Answer::Code)
                );
            }

            // No kind, or one we can't read: answer nothing at all.
            assert_eq!(
                classify("Password:", None, &keys(), has_code),
                Answer::Decline
            );
            assert_eq!(
                classify("Verification code: ", None, &keys(), has_code),
                Answer::Decline
            );
        }
    }

    #[test]
    fn a_key_host_with_no_identity_files_answers_nothing() {
        // Fail closed: without the list we can't tell our own key's prompt from a forged one.
        let prompt = format!("Enter passphrase for key '{KEY}': ");
        assert_eq!(
            classify(&prompt, Some(SecretKind::Passphrase), &[], true),
            Answer::Decline
        );
    }

    #[test]
    fn kind_round_trips_through_the_environment() {
        for k in [
            SecretKind::Password,
            SecretKind::Passphrase,
            SecretKind::Agent,
        ] {
            assert_eq!(SecretKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(SecretKind::parse(""), None);
        assert_eq!(SecretKind::parse("PASSWORD"), None);
    }

    #[test]
    fn declines_host_key_prompts() {
        // These never arrive (StrictHostKeyChecking=accept-new), but if one did it is not
        // secret-shaped, so with no code queued it is declined.
        assert_eq!(
            classify(
                "Are you sure you want to continue connecting (yes/no/[fingerprint])? ",
                Some(SecretKind::Password),
                &keys(),
                false
            ),
            Answer::Decline
        );
    }

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sshelf-marker-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The same secret prompt twice in one connect means the first answer was refused.
    #[test]
    fn a_repeated_prompt_in_one_connect_is_reported_once_and_declined() {
        let dir = scratch();
        let id = ulid::Ulid::new().to_string();
        let prompt = "tester@host's password: ";

        assert_eq!(mark(&dir, &id, prompt), Marker::First);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(dir.join(format!("askpass-{id}"))).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(mark(&dir, &id, prompt), Marker::Repeat { warn: true });
        // A password prompt comes back once per attempt ssh has left; say it once.
        assert_eq!(mark(&dir, &id, prompt), Marker::Repeat { warn: false });
        // A different secret prompt in the same connect (another key file) is not a refusal.
        assert_eq!(
            mark(&dir, &id, "Enter passphrase for key '/other/key': "),
            Marker::Other
        );
        // The next connect starts clean.
        assert_eq!(
            mark(&dir, &ulid::Ulid::new().to_string(), prompt),
            Marker::First
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_marker_that_cannot_be_made_fails_open() {
        let dir = scratch();
        assert_eq!(mark(&dir, "", "Password:"), Marker::Unavailable);
        assert_eq!(mark(&dir, "../escape", "Password:"), Marker::Unavailable);
        assert_eq!(
            mark(&dir.join("missing"), "01ABCDEF", "Password:"),
            Marker::Unavailable
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_stale_markers_are_swept() {
        let dir = scratch();
        std::fs::write(dir.join("askpass-01OLD"), "Password:\n").unwrap();
        std::fs::create_dir(dir.join("mux-01SESSION")).unwrap();
        std::fs::write(dir.join("unrelated"), "").unwrap();

        remove_stale_markers(&dir, SystemTime::now());
        assert!(dir.join("askpass-01OLD").exists(), "a fresh marker stays");

        remove_stale_markers(&dir, SystemTime::now() + Duration::from_secs(11 * 60));
        assert!(!dir.join("askpass-01OLD").exists());
        assert!(dir.join("mux-01SESSION").is_dir());
        assert!(dir.join("unrelated").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
