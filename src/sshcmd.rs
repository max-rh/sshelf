//! Turning a working `ssh` command line into a saved host (`sshelf add --from-ssh`, D-034).
//!
//! The line is read with OpenSSH's own option grammar (`ssh(1)`): what the host model has a
//! field for is mapped onto it, what it doesn't is kept verbatim in `extra_args`, and the few
//! options that would change what a saved connection does are dropped with a note saying why.
//!
//! Pure functions. The only outside input is the working directory a relative `-i` path is
//! resolved against, and [`parse`] reads that once before handing it in. Nothing here reads
//! `~/.ssh/config` or resolves an alias: an alias stays an alias, and the user's own ssh config
//! still resolves it at connect time.

use std::path::{Component, Path, PathBuf};

use crate::display;
use crate::model::{AuthMethod, Host};

/// `ssh` options that take no value (OpenSSH 9.x).
const BOOLEAN_FLAGS: &str = "46AaCfGgKkMNnqsTtVvXxYy";
/// `ssh` options that take a value, attached (`-p2222`) or as the next word (`-p 2222`).
const VALUE_FLAGS: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// The options a saved host must not keep, and the reason printed for each.
fn dropped_reason(flag: char) -> Option<&'static str> {
    Some(match flag {
        'v' => "verbose output is for one debugging run, not a saved host",
        'q' => "quiet mode would hide ssh's own errors on every connect",
        'G' => "it prints the resolved config and never connects",
        'V' => "it prints the ssh version and never connects",
        'Q' => "it lists supported algorithms and never connects",
        'O' => "it controls a running master and never opens a session",
        'S' => "a control socket belongs to one session",
        'E' => "a log file belongs to one run",
        'M' => "master mode is for scripted multiplexing, not an interactive login",
        'N' => "a connect with no remote shell would sit there doing nothing",
        'f' => "backgrounding would take the session away from your terminal",
        'n' => "reading stdin from /dev/null breaks an interactive session",
        'g' => "it opens local forwards to other machines; add it under extra args if you mean it",
        's' => "a subsystem request is not an interactive login",
        _ => return None,
    })
}

const STRICT_HOST_KEY_NOTE: &str = "sshelf passes accept-new on every connect";

/// Why a command line could not be turned into a host.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("the ssh command is empty")]
    Empty,
    #[error("the ssh command has an unbalanced quote or a trailing backslash")]
    Unbalanced,
    #[error("the ssh command contains control characters")]
    Control,
    #[error(
        "`{0}` is not ssh: sshelf saves connections, not commands, so pass only the ssh part \
         (if `{0}` is the host's name, put it before --from-ssh)"
    )]
    NotSsh(String),
    #[error("`{0}` is not an ssh option")]
    UnknownFlag(String),
    #[error("`-{0}` needs a value")]
    MissingValue(char),
    #[error("the ssh command has no destination (user@host)")]
    NoDestination,
    #[error(
        "the ssh command runs a remote command (`{0}`); a saved host has no command, so drop \
         it and try again"
    )]
    RemoteCommand(String),
    #[error("`{0}` is not a port (1-65535)")]
    BadPort(String),
    #[error("`{0}` is not a destination sshelf can read ([user@]host or ssh://[user@]host[:port])")]
    BadDestination(String),
}

/// A command line, read. Every field is what `ssh` itself would have used; nothing is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSsh {
    /// The destination's host token, verbatim (an ssh alias stays an alias).
    pub destination: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub auth: AuthMethod,
    /// Every `-i`, in order. `~` kept as typed; a relative path made absolute.
    pub identities: Vec<String>,
    /// `-J`, split on `,`.
    pub jumps: Vec<String>,
    /// Everything kept verbatim for `extra_args`, one word per entry, in order.
    pub extras: Vec<String>,
    /// One line per thing that was dropped or rewritten, for the caller to print.
    pub notes: Vec<String>,
}

impl ParsedSsh {
    /// The host this line describes, named `name` or else after the destination. Never touches
    /// the secret store: a command line never carries a secret.
    pub fn into_host(self, name: Option<String>) -> Host {
        let name = name.unwrap_or_else(|| self.destination.clone());
        let mut h = Host::new(name, self.destination);
        h.user = self.user;
        h.port = self.port;
        h.auth = self.auth;
        h.identity_files = self.identities;
        h.jump_hosts = self.jumps;
        h.extra_args = (!self.extras.is_empty()).then(|| join_words(&self.extras));
        h
    }
}

/// Join the words so the result survives the `shlex::split` that `ssh::build_args` runs on
/// `extra_args` at connect time.
fn join_words(words: &[String]) -> String {
    words
        .iter()
        .map(|w| quoted(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Read one ssh command line. A relative `-i` is resolved against the current directory.
pub fn parse(line: &str) -> Result<ParsedSsh, ParseError> {
    let cwd = std::env::current_dir().ok();
    parse_in(line, cwd.as_deref())
}

/// One parsed option, in command-line order.
enum Opt {
    Flag(char),
    Value(char, String),
}

/// [`parse`] with the working directory handed in, so the tests don't depend on where they run.
fn parse_in(line: &str, cwd: Option<&Path>) -> Result<ParsedSsh, ParseError> {
    if line.trim().is_empty() {
        return Err(ParseError::Empty);
    }
    let words = shlex::split(line).ok_or(ParseError::Unbalanced)?;
    if words.iter().any(|w| display::has_control(w)) {
        return Err(ParseError::Control);
    }
    let mut words = words.as_slice();
    match words.first() {
        None => return Err(ParseError::Empty),
        Some(first) if first == "ssh" || first.ends_with("/ssh") => words = &words[1..],
        Some(first) if first.starts_with('-') && first.len() > 1 => {}
        Some(first) => return Err(ParseError::NotSsh(first.clone())),
    }

    let (opts, destination) = read_options(words)?;
    let destination = destination.ok_or(ParseError::NoDestination)?;
    map(opts, &destination, cwd)
}

/// Walk the words with ssh's getopt rules: booleans combine (`-At`), a value is attached or the
/// next word, `--` ends options. Like `ssh` itself, options are read again after the destination
/// (`ssh host -p 2222` works), and the first plain word after it starts a remote command.
fn read_options(words: &[String]) -> Result<(Vec<Opt>, Option<String>), ParseError> {
    let mut opts = Vec::new();
    let mut destination: Option<String> = None;
    let mut terminated = false;
    let mut i = 0;
    while i < words.len() {
        let word = &words[i];
        if !terminated && word == "--" {
            terminated = true;
            i += 1;
            continue;
        }
        if !terminated && word.starts_with('-') && word.len() > 1 {
            if word.starts_with("--") {
                return Err(ParseError::UnknownFlag(word.clone()));
            }
            let chars: Vec<char> = word[1..].chars().collect();
            for (j, &flag) in chars.iter().enumerate() {
                if BOOLEAN_FLAGS.contains(flag) {
                    opts.push(Opt::Flag(flag));
                } else if VALUE_FLAGS.contains(flag) {
                    let attached: String = chars[j + 1..].iter().collect();
                    let value = if attached.is_empty() {
                        i += 1;
                        words
                            .get(i)
                            .cloned()
                            .ok_or(ParseError::MissingValue(flag))?
                    } else {
                        attached
                    };
                    opts.push(Opt::Value(flag, value));
                    break;
                } else {
                    return Err(ParseError::UnknownFlag(format!("-{flag}")));
                }
            }
            i += 1;
            continue;
        }
        if destination.is_some() {
            let command = shlex::try_join(words[i..].iter().map(String::as_str))
                .unwrap_or_else(|_| words[i..].join(" "));
            return Err(ParseError::RemoteCommand(command));
        }
        destination = Some(word.clone());
        i += 1;
    }
    Ok((opts, destination))
}

/// One word or option pair headed for `extra_args`. `auth_hint` marks the two `-o` values that
/// are consumed when they make this a password host.
struct Extra {
    words: Vec<String>,
    auth_hint: bool,
}

fn map(opts: Vec<Opt>, destination: &str, cwd: Option<&Path>) -> Result<ParsedSsh, ParseError> {
    let (dest_user, host, url_port) = parse_destination(destination)?;
    let mut user: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut identities = Vec::new();
    let mut jumps = Vec::new();
    let mut extras: Vec<Extra> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    // First value wins for each keyword, as in ssh.
    let mut password_auth: Option<bool> = None;
    let mut preferred_password: Option<bool> = None;

    let mut note = |text: String| {
        if !notes.contains(&text) {
            notes.push(text);
        }
    };

    for opt in opts {
        match opt {
            Opt::Flag(flag) => match dropped_reason(flag) {
                Some(reason) => note(format!("dropped -{flag}: {reason}")),
                None => extras.push(Extra {
                    words: vec![format!("-{flag}")],
                    auth_hint: false,
                }),
            },
            Opt::Value('i', path) => identities.push(absolute_key(path, cwd, &mut note)),
            Opt::Value('l', login) => {
                user.get_or_insert(login);
            }
            Opt::Value('p', value) => {
                let parsed = parse_port(&value)?;
                port.get_or_insert(parsed);
            }
            Opt::Value('J', chain) => jumps.extend(
                chain
                    .split(',')
                    .map(str::trim)
                    .filter(|j| !j.is_empty())
                    .map(str::to_string),
            ),
            Opt::Value('o', option) => {
                let (key, value) = split_option(&option);
                if key.eq_ignore_ascii_case("StrictHostKeyChecking") {
                    note(format!(
                        "dropped -o {}: {STRICT_HOST_KEY_NOTE}",
                        quoted(&option)
                    ));
                    continue;
                }
                let auth_hint = if key.eq_ignore_ascii_case("PasswordAuthentication") {
                    password_auth.get_or_insert(value.eq_ignore_ascii_case("yes"));
                    true
                } else if key.eq_ignore_ascii_case("PreferredAuthentications") {
                    let first = value.split(',').next().unwrap_or("").trim();
                    preferred_password.get_or_insert(first.eq_ignore_ascii_case("password"));
                    true
                } else {
                    false
                };
                extras.push(Extra {
                    words: vec!["-o".to_string(), option],
                    auth_hint,
                });
            }
            Opt::Value(flag, value) => match dropped_reason(flag) {
                Some(reason) => note(format!("dropped -{flag} {}: {reason}", quoted(&value))),
                None => extras.push(Extra {
                    words: vec![format!("-{flag}"), value],
                    auth_hint: false,
                }),
            },
        }
    }

    let auth = if !identities.is_empty() {
        AuthMethod::Key
    } else if password_auth == Some(true) || preferred_password == Some(true) {
        extras.retain(|e| !e.auth_hint);
        AuthMethod::Password
    } else {
        AuthMethod::Agent
    };

    Ok(ParsedSsh {
        destination: host,
        user: user.or(dest_user),
        port: port.or(url_port),
        auth,
        identities,
        jumps,
        extras: extras.into_iter().flat_map(|e| e.words).collect(),
        notes,
    })
}

/// A word as a person would retype it: bare when splitting it gives it straight back, quoted
/// otherwise. `shlex` on its own also quotes `=`, which turns every `-o Key=Value` into noise.
/// Control characters were refused before getting here, and they are the only thing
/// `try_quote` rejects.
fn quoted(word: &str) -> String {
    if shlex::split(word).is_some_and(|split| split == [word]) {
        return word.to_string();
    }
    shlex::try_quote(word).map_or_else(|_| word.to_string(), |q| q.into_owned())
}

/// `Key=Value`, `Key Value` and `Key = Value` all mean the same thing to ssh.
fn split_option(option: &str) -> (&str, &str) {
    match option.find(|c: char| c == '=' || c.is_whitespace()) {
        Some(at) => (
            &option[..at],
            option[at..].trim_start_matches(|c: char| c == '=' || c.is_whitespace()),
        ),
        None => (option, ""),
    }
}

fn parse_port(value: &str) -> Result<u16, ParseError> {
    match value.parse::<u16>() {
        Ok(p) if p != 0 => Ok(p),
        _ => Err(ParseError::BadPort(value.to_string())),
    }
}

/// A key path as it should be saved. `~` stays as typed, since the connect path expands it; an
/// absolute path is kept; a relative one is resolved now, because connects happen from anywhere.
fn absolute_key(path: String, cwd: Option<&Path>, note: &mut impl FnMut(String)) -> String {
    if path.starts_with('/') || path.starts_with('~') {
        return path;
    }
    let Some(cwd) = cwd else {
        return path;
    };
    let resolved: PathBuf = cwd
        .join(&path)
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    let resolved = resolved.to_string_lossy().into_owned();
    note(format!(
        "saved -i {} as {}: a relative key path would break when you connect from another directory",
        quoted(&path),
        quoted(&resolved)
    ));
    resolved
}

/// `[user@]host` or `ssh://[user@]host[:port]` (a bracketed IPv6 literal allowed in the URL).
fn parse_destination(dest: &str) -> Result<(Option<String>, String, Option<u16>), ParseError> {
    let bad = || ParseError::BadDestination(dest.to_string());
    let (user, host, port) = if let Some(rest) = dest.strip_prefix("ssh://") {
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        let (user, hostport) = match rest.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, rest),
        };
        let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
            let (addr, after) = inner.split_once(']').ok_or_else(bad)?;
            match after {
                "" => (addr, None),
                p => (addr, Some(p.strip_prefix(':').ok_or_else(bad)?)),
            }
        } else {
            match hostport.rsplit_once(':') {
                Some((h, _)) if h.contains(':') => return Err(bad()),
                Some((h, p)) => (h, Some(p)),
                None => (hostport, None),
            }
        };
        let port = port.map(parse_port).transpose()?;
        (user, host, port)
    } else {
        match dest.rsplit_once('@') {
            Some((u, h)) => (Some(u), h, None),
            None => (None, dest, None),
        }
    };
    if user.is_some_and(str::is_empty) || host.is_empty() || host.starts_with('-') {
        return Err(bad());
    }
    Ok((user.map(str::to_string), host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> &'static Path {
        Path::new("/work/dir")
    }

    fn parsed(line: &str) -> ParsedSsh {
        parse_in(line, Some(cwd())).unwrap_or_else(|e| panic!("{line:?} failed: {e}"))
    }

    fn host(line: &str) -> Host {
        parsed(line).into_host(None)
    }

    fn err(line: &str) -> ParseError {
        match parse_in(line, Some(cwd())) {
            Err(e) => e,
            Ok(p) => panic!("{line:?} should not parse, got {p:?}"),
        }
    }

    #[test]
    fn the_motivating_line() {
        let p = parsed(
            "ssh -i ~/Downloads/dev-ooblek-privco.pem -o StrictHostKeyChecking=no ubuntu@44.196.235.116",
        );
        assert_eq!(
            p.notes,
            vec!["dropped -o StrictHostKeyChecking=no: sshelf passes accept-new on every connect"]
        );
        let h = p.into_host(None);
        assert_eq!(h.name, "44.196.235.116");
        assert_eq!(h.hostname, "44.196.235.116");
        assert_eq!(h.user.as_deref(), Some("ubuntu"));
        assert_eq!(h.port, None);
        assert_eq!(h.auth, AuthMethod::Key);
        assert_eq!(h.identity_files, vec!["~/Downloads/dev-ooblek-privco.pem"]);
        assert!(h.jump_hosts.is_empty());
        assert_eq!(h.extra_args, None);
    }

    /// The table: a line, and the fields it must produce.
    #[test]
    fn command_lines_map_to_hosts() {
        struct Case {
            line: &'static str,
            hostname: &'static str,
            user: Option<&'static str>,
            port: Option<u16>,
            auth: AuthMethod,
            identities: &'static [&'static str],
            jumps: &'static [&'static str],
            extra: Option<&'static str>,
        }
        let cases = [
            // `-p` wins over the URL's port; the URL still supplies user and host.
            Case {
                line: "ssh -p 2222 ssh://deploy@web.example.com:2200",
                hostname: "web.example.com",
                user: Some("deploy"),
                port: Some(2222),
                auth: AuthMethod::Agent,
                identities: &[],
                jumps: &[],
                extra: None,
            },
            Case {
                line: "ssh ssh://deploy@web.example.com:2200",
                hostname: "web.example.com",
                user: Some("deploy"),
                port: Some(2200),
                auth: AuthMethod::Agent,
                identities: &[],
                jumps: &[],
                extra: None,
            },
            // `-l` wins over `user@`.
            Case {
                line: "ssh -l admin root@db1",
                hostname: "db1",
                user: Some("admin"),
                port: None,
                auth: AuthMethod::Agent,
                identities: &[],
                jumps: &[],
                extra: None,
            },
            Case {
                line: "ssh -J bastion,jump2:2222 app",
                hostname: "app",
                user: None,
                port: None,
                auth: AuthMethod::Agent,
                identities: &[],
                jumps: &["bastion", "jump2:2222"],
                extra: None,
            },
            // Combined booleans are kept, one flag per word.
            Case {
                line: "ssh -At -X box",
                hostname: "box",
                user: None,
                port: None,
                auth: AuthMethod::Agent,
                identities: &[],
                jumps: &[],
                extra: Some("-A -t -X"),
            },
            // Attached values, including a value flag at the end of a boolean group.
            Case {
                line: "ssh -p2222 -ikey.pem -Ai/abs/other -oServerAliveInterval=30 u@h",
                hostname: "h",
                user: Some("u"),
                port: Some(2222),
                auth: AuthMethod::Key,
                identities: &["/work/dir/key.pem", "/abs/other"],
                jumps: &[],
                extra: Some("-A -o ServerAliveInterval=30"),
            },
            // `--` ends options; the next word is the destination.
            Case {
                line: "ssh -C -- me@host",
                hostname: "host",
                user: Some("me"),
                port: None,
                auth: AuthMethod::Agent,
                identities: &[],
                jumps: &[],
                extra: Some("-C"),
            },
            // ssh reads options after the destination too.
            Case {
                line: "ssh me@host -p 2200",
                hostname: "host",
                user: Some("me"),
                port: Some(2200),
                auth: AuthMethod::Agent,
                identities: &[],
                jumps: &[],
                extra: None,
            },
            // A bracketed IPv6 literal in the URL form.
            Case {
                line: "ssh ssh://ops@[2001:db8::1]:2022",
                hostname: "2001:db8::1",
                user: Some("ops"),
                port: Some(2022),
                auth: AuthMethod::Agent,
                identities: &[],
                jumps: &[],
                extra: None,
            },
            // Password auth is read off the two `-o` values, which are then consumed.
            Case {
                line: "ssh -o PreferredAuthentications=password -o PubkeyAuthentication=no u@legacy",
                hostname: "legacy",
                user: Some("u"),
                port: None,
                auth: AuthMethod::Password,
                identities: &[],
                jumps: &[],
                extra: Some("-o PubkeyAuthentication=no"),
            },
            Case {
                line: "/usr/bin/ssh -o passwordauthentication=YES u@legacy",
                hostname: "legacy",
                user: Some("u"),
                port: None,
                auth: AuthMethod::Password,
                identities: &[],
                jumps: &[],
                extra: None,
            },
            // First value wins, as in ssh: `no` first means not a password host.
            Case {
                line: "ssh -o PasswordAuthentication=no -o PasswordAuthentication=yes u@h",
                hostname: "h",
                user: Some("u"),
                port: None,
                auth: AuthMethod::Agent,
                identities: &[],
                jumps: &[],
                extra: Some("-o PasswordAuthentication=no -o PasswordAuthentication=yes"),
            },
            // With a key, the password hints are just options and stay.
            Case {
                line: "ssh -i ~/.ssh/k -o PasswordAuthentication=yes u@h",
                hostname: "h",
                user: Some("u"),
                port: None,
                auth: AuthMethod::Key,
                identities: &["~/.ssh/k"],
                jumps: &[],
                extra: Some("-o PasswordAuthentication=yes"),
            },
            // Forwards and a config file ride along, in order, quoted where needed.
            Case {
                line: "ssh -F '/my configs/ssh' -L 8080:localhost:80 -D 1080 alias",
                hostname: "alias",
                user: None,
                port: None,
                auth: AuthMethod::Agent,
                identities: &[],
                jumps: &[],
                extra: Some("-F '/my configs/ssh' -L 8080:localhost:80 -D 1080"),
            },
            // A line may start with its options when there is no `ssh` in front.
            Case {
                line: "-p 22 host",
                hostname: "host",
                user: None,
                port: Some(22),
                auth: AuthMethod::Agent,
                identities: &[],
                jumps: &[],
                extra: None,
            },
        ];
        for c in cases {
            let h = host(c.line);
            assert_eq!(h.hostname, c.hostname, "{}", c.line);
            assert_eq!(h.user.as_deref(), c.user, "{}", c.line);
            assert_eq!(h.port, c.port, "{}", c.line);
            assert_eq!(h.auth, c.auth, "{}", c.line);
            assert_eq!(h.identity_files, c.identities, "{}", c.line);
            assert_eq!(h.jump_hosts, c.jumps, "{}", c.line);
            assert_eq!(h.extra_args.as_deref(), c.extra, "{}", c.line);
        }
    }

    #[test]
    fn lines_that_are_refused() {
        assert_eq!(err(""), ParseError::Empty);
        assert_eq!(err("   "), ParseError::Empty);
        assert_eq!(err("ssh 'user@host"), ParseError::Unbalanced);
        assert_eq!(err("sudo ssh host"), ParseError::NotSsh("sudo".into()));
        assert_eq!(
            err("FOO=bar ssh host"),
            ParseError::NotSsh("FOO=bar".into())
        );
        assert_eq!(err("web"), ParseError::NotSsh("web".into()));
        assert_eq!(err("ssh -Z host"), ParseError::UnknownFlag("-Z".into()));
        assert_eq!(
            err("ssh --verbose host"),
            ParseError::UnknownFlag("--verbose".into())
        );
        assert_eq!(err("ssh host -p"), ParseError::MissingValue('p'));
        assert_eq!(err("ssh -v"), ParseError::NoDestination);
        assert_eq!(err("ssh"), ParseError::NoDestination);
        assert_eq!(
            err("ssh host uptime -p"),
            ParseError::RemoteCommand("uptime -p".into())
        );
        assert_eq!(
            err("ssh -- host -p 22"),
            ParseError::RemoteCommand("-p 22".into()),
            "after `--` nothing is an option any more"
        );
        assert_eq!(err("ssh -p 0 host"), ParseError::BadPort("0".into()));
        assert_eq!(err("ssh -p ssh host"), ParseError::BadPort("ssh".into()));
        assert_eq!(
            err("ssh ssh://host:99999"),
            ParseError::BadPort("99999".into())
        );
        assert!(matches!(err("ssh @host"), ParseError::BadDestination(_)));
        assert!(matches!(
            err("ssh ssh://2001:db8::1"),
            ParseError::BadDestination(_)
        ));
        assert_eq!(err("ssh 'host\u{1b}[2J'"), ParseError::Control);
    }

    #[test]
    fn a_remote_command_error_says_what_to_drop() {
        let msg = err("ssh host 'tail -f /var/log/syslog'").to_string();
        assert!(msg.contains("tail -f /var/log/syslog"), "{msg}");
        assert!(msg.contains("drop it"), "{msg}");
        assert!(
            !msg.contains('\u{2014}'),
            "no em dashes in user-facing text: {msg}"
        );
    }

    #[test]
    fn a_relative_key_is_made_absolute_and_says_so() {
        let p = parsed("ssh -i ./keys/dev.pem -i ../shared.pem u@h");
        assert_eq!(
            p.identities,
            vec!["/work/dir/keys/dev.pem", "/work/dir/../shared.pem"]
        );
        assert_eq!(p.notes.len(), 2);
        assert!(
            p.notes[0].starts_with("saved -i ./keys/dev.pem as /work/dir/keys/dev.pem: "),
            "{:?}",
            p.notes
        );
    }

    #[test]
    fn dropped_options_get_one_note_each() {
        let p = parsed(
            "ssh -vvv -v -N -f -S /tmp/ctl -o StrictHostKeyChecking=no -o 'StrictHostKeyChecking accept-new' h",
        );
        assert_eq!(
            p.notes,
            vec![
                "dropped -v: verbose output is for one debugging run, not a saved host",
                "dropped -N: a connect with no remote shell would sit there doing nothing",
                "dropped -f: backgrounding would take the session away from your terminal",
                "dropped -S /tmp/ctl: a control socket belongs to one session",
                "dropped -o StrictHostKeyChecking=no: sshelf passes accept-new on every connect",
                "dropped -o 'StrictHostKeyChecking accept-new': sshelf passes accept-new on every connect",
            ]
        );
        assert!(p.extras.is_empty(), "{:?}", p.extras);
    }

    /// `extra_args` is split again with `shlex` at connect time, so every word must come back
    /// exactly as it was parsed.
    #[test]
    fn extra_args_round_trip_through_shlex() {
        let p = parsed(
            "ssh -o 'ProxyCommand=ssh -W %h:%p gw' -F \"/a b/c\" -L '127.0.0.1:8080:it'\\''s:80' -o SetEnv=\"X=1 2\" h",
        );
        let joined = p.clone().into_host(None).extra_args.unwrap();
        assert_eq!(shlex::split(&joined).unwrap(), p.extras);
        assert_eq!(
            p.extras,
            vec![
                "-o",
                "ProxyCommand=ssh -W %h:%p gw",
                "-F",
                "/a b/c",
                "-L",
                "127.0.0.1:8080:it's:80",
                "-o",
                "SetEnv=X=1 2",
            ]
        );
    }

    #[test]
    fn the_name_defaults_to_the_destination_and_the_id_is_fresh() {
        let a = parsed("ssh deploy@web1").into_host(None);
        let b = parsed("ssh deploy@web1").into_host(Some("prod-web".into()));
        assert_eq!(a.name, "web1");
        assert_eq!(b.name, "prod-web");
        assert_ne!(a.id, b.id);
        assert!(ulid::Ulid::from_string(&a.id).is_ok());
    }
}
