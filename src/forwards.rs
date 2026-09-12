//! Background SSH port-forwards that outlive sshelf.
//!
//! A forward is a single detached `ssh -N -L|-R|-D …` process. We reuse the connect/transfer
//! machinery ([`ssh::build_args`] + [`ssh::configure_askpass`]) so keys, agents, ProxyJump and
//! stored passwords all work exactly as a normal connect — then detach the child into its **own
//! process group** ([`std::os::unix::process::CommandExt::process_group`]) with null stdin/stdout
//! so it survives both sshelf exiting (orphaned → reparented to init) and the terminal closing
//! (its own process group never receives the shell's hangup). Nothing here kills a forward on
//! drop or on app shutdown — that is what keeps it running.
//!
//! There is no daemon: the running `ssh` processes are the source of truth and [`ForwardsState`]
//! (`forwards.json`) is just a remembered list of PIDs. [`reconcile`] re-validates every PID
//! against the OS (via `ps`), so a forward that ends — stopped here, `kill`ed elsewhere, or
//! dropped on its own — leaves the ledger; only forwards that are *still actually running*
//! persist (across sshelf's own exit and relaunch).

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::Host;
use crate::ssh;
use crate::state::now_unix;
use crate::store::atomic_write;

/// Poll cadence while waiting for a freshly-spawned forward to come up or fail.
const POLL: Duration = Duration::from_millis(100);
/// How long to wait for `ssh` to authenticate + bind before treating a still-running child as
/// "up". A bind/auth failure makes `ssh` (with `ExitOnForwardFailure=yes`) exit well within this;
/// a child still alive at the deadline is taken as up (and `reconcile` self-heals a late death).
const READINESS_GRACE: Duration = Duration::from_millis(2500);
/// After `SIGTERM`, wait at most this long for the forward to die before escalating to `SIGKILL`.
const KILL_GRACE: Duration = Duration::from_millis(600);

/// Which direction a forward tunnels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardKind {
    /// `-L` — bind a local port that tunnels to a host reachable from the server.
    Local,
    /// `-R` — bind a port on the server that tunnels back to a host reachable from us.
    Remote,
    /// `-D` — bind a local SOCKS proxy that routes through the server.
    Dynamic,
}

impl ForwardKind {
    /// Every kind, in the order the popup chooser cycles them (Local is the default).
    pub const ALL: [ForwardKind; 3] = [
        ForwardKind::Local,
        ForwardKind::Remote,
        ForwardKind::Dynamic,
    ];

    /// The `ssh` flag that selects this kind.
    pub fn flag(self) -> &'static str {
        match self {
            ForwardKind::Local => "-L",
            ForwardKind::Remote => "-R",
            ForwardKind::Dynamic => "-D",
        }
    }

    /// Human label for the chooser.
    pub fn label(self) -> &'static str {
        match self {
            ForwardKind::Local => "Local",
            ForwardKind::Remote => "Remote",
            ForwardKind::Dynamic => "Dynamic",
        }
    }

    /// Single-letter tag used in the manager's display string.
    fn tag(self) -> char {
        match self {
            ForwardKind::Local => 'L',
            ForwardKind::Remote => 'R',
            ForwardKind::Dynamic => 'D',
        }
    }
}

/// The ports/hosts of one forward. Field roles depend on the kind:
/// - **Local** (`-L`): `listen_port` is the local port; `target_host`:`target_port` is the
///   destination reached *from the server*.
/// - **Remote** (`-R`): `listen_port` is the port bound *on the server*; `target_host`:`target_port`
///   is the destination reached *from us*.
/// - **Dynamic** (`-D`): only `listen_port` (the local SOCKS port); the target fields are unused.
///
/// `bind` is the listen interface, defaulting to loopback (`127.0.0.1`); `target_host` defaults to
/// `localhost`. These defaults are applied in [`ForwardSpec::spec_string`], not stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    pub listen_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_port: Option<u16>,
}

impl ForwardSpec {
    fn bind_or_default(&self) -> &str {
        self.bind
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("127.0.0.1")
    }

    fn target_or_default(&self) -> &str {
        self.target_host
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("localhost")
    }

    /// The value passed after the `-L`/`-R`/`-D` flag, with defaults applied.
    pub fn spec_string(&self, kind: ForwardKind) -> String {
        let bind = self.bind_or_default();
        match kind {
            ForwardKind::Dynamic => format!("{bind}:{}", self.listen_port),
            ForwardKind::Local | ForwardKind::Remote => format!(
                "{bind}:{}:{}:{}",
                self.listen_port,
                self.target_or_default(),
                self.target_port.unwrap_or(0),
            ),
        }
    }

    /// A compact human description for the forwards manager (e.g. `L  127.0.0.1:8080 → db:3306`).
    pub fn display_string(&self, kind: ForwardKind) -> String {
        let bind = self.bind_or_default();
        match kind {
            ForwardKind::Dynamic => format!("{}  {bind}:{} (SOCKS)", kind.tag(), self.listen_port),
            ForwardKind::Local | ForwardKind::Remote => format!(
                "{}  {bind}:{} → {}:{}",
                kind.tag(),
                self.listen_port,
                self.target_or_default(),
                self.target_port.unwrap_or(0),
            ),
        }
    }

    /// Validate the user-entered ports. Privileged ports (<1024) are *not* rejected here — they
    /// surface as a friendly bind error from `ssh` if the OS refuses them.
    pub fn validate(&self, kind: ForwardKind) -> Result<(), String> {
        if self.listen_port == 0 {
            return Err("listen port must be between 1 and 65535".into());
        }
        if matches!(kind, ForwardKind::Local | ForwardKind::Remote)
            && !matches!(self.target_port, Some(p) if p != 0)
        {
            return Err("destination port must be between 1 and 65535".into());
        }
        Ok(())
    }
}

/// One active forward, as recorded in `forwards.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardEntry {
    /// Stable id (ULID); also names the forward's stderr log file.
    pub id: String,
    /// The originating [`Host::id`] (so secrets/site could be re-resolved later).
    pub host_id: String,
    /// The host's display name, snapshotted (the host may be renamed/deleted afterwards).
    pub host_name: String,
    pub kind: ForwardKind,
    pub spec: ForwardSpec,
    /// Precomputed display string for the manager.
    pub display: String,
    pub pid: i32,
    pub started_at: i64,
}

impl ForwardEntry {
    /// The token that must appear in the live process's command line for it to be *our* forward
    /// (guards against PID reuse).
    fn spec_token(&self) -> String {
        self.spec.spec_string(self.kind)
    }
}

/// The whole `forwards.json`: a flat list of active forwards.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ForwardsState {
    pub forwards: Vec<ForwardEntry>,
}

impl ForwardsState {
    /// Load state; a missing/empty file yields default (empty) state.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self).context("serializing forwards")?;
        atomic_write(path, text.as_bytes(), 0o600)
    }
}

/// The `-L|-R|-D <spec>` arguments for one forward (no IO; unit-tested).
pub fn forward_args(kind: ForwardKind, spec: &ForwardSpec) -> Vec<String> {
    vec![kind.flag().to_string(), spec.spec_string(kind)]
}

/// The full argv (excluding the program name) for a forward: the constant `-N
/// -o ExitOnForwardFailure=yes`, the forward spec, then the host's normal `ssh` args.
///
/// `askpass` says whether the helper will be wired for this forward, which is what decides how
/// the jump chain is expressed (see [`ssh::jump_plan`]).
pub fn build_forward_command(
    host: &Host,
    kind: ForwardKind,
    spec: &ForwardSpec,
    askpass: bool,
) -> Vec<String> {
    let mut a = vec![
        "-N".to_string(),
        "-o".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
    ];
    a.extend(forward_args(kind, spec));
    a.extend(ssh::no_prompt_args(askpass));
    a.extend(ssh::build_args(host, true, askpass));
    a
}

/// Where a forward's stderr is logged (a regular file, so a long-lived `ssh` never gets SIGPIPE
/// from a closed pipe). Derived from the id, so `reconcile`/`kill` can clean it up too.
///
/// Under our own data dir, not `/tmp`: the log carries the hostname, the forward spec, the user's
/// `extra_args` and whatever ssh says about the connection, and a predictable name in a
/// world-writable directory is a name anyone on the box can claim first.
///
/// The data dir is resolved here rather than passed in because [`kill`] and [`reconcile`] delete
/// these logs as best-effort cleanup with nowhere to report a failure — the askpass helper
/// resolves its own paths for the same reason. `None` only when there is no home directory.
fn log_path_for(id: &str) -> Option<PathBuf> {
    let paths = crate::paths::Paths::resolve().ok()?;
    Some(log_path_in(&paths.data_dir, id))
}

/// The log path for `id` under an explicit data dir — split out so it can be checked without
/// resolving (and therefore reading) the environment.
fn log_path_in(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join("logs").join(format!("fwd-{id}.log"))
}

/// Delete a finished forward's log. Best-effort: a missing file is the normal case.
///
/// It also unlinks the pre-0.13 `/tmp/sshelf-fwd-<id>.log` that older builds wrote. Those were
/// left at the umask default in a shared directory and hold the same connection details as the
/// new ones, so an upgrade should take them with it rather than leave them readable forever. The
/// legacy name is only ever *removed*, never opened or written — unlinking a name removes the
/// name, so a symlink someone planted there dies without its target being touched. Drop this a
/// release or two after 0.13.
fn remove_log(id: &str) {
    if let Some(log) = log_path_for(id) {
        let _ = std::fs::remove_file(log);
    }
    let _ = std::fs::remove_file(std::env::temp_dir().join(format!("sshelf-fwd-{id}.log")));
}

/// Create a forward's stderr log 0600, inside a 0700 `logs` directory it creates if needed.
/// Exclusive, so a file or symlink already sitting at the name fails the spawn instead of being
/// written through.
fn create_log_file(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        crate::paths::ensure_private_dir(parent, true)?;
    }
    crate::store::create_exclusive(path, 0o600)
}

/// Spawn a detached forward for `host` (already resolved with site defaults) and wait briefly to
/// catch an immediate bind/auth failure. On success returns the [`ForwardEntry`] to record; on
/// failure returns a friendly message (the popup keeps it open so the user can fix a field).
pub fn spawn_forward(
    host: &Host,
    host_name: &str,
    has_secret: bool,
    kind: ForwardKind,
    spec: ForwardSpec,
) -> Result<ForwardEntry, String> {
    spec.validate(kind)?;

    let id = ulid::Ulid::new().to_string();
    let log_path = log_path_for(&id)
        .ok_or_else(|| "could not determine the data directory for the forward log".to_string())?;
    let errfile = create_log_file(&log_path)
        .map_err(|e| format!("could not create forward log {}: {e}", log_path.display()))?;

    let mut cmd = Command::new("ssh");
    cmd.args(build_forward_command(host, kind, &spec, has_secret));
    ssh::configure_askpass(&mut cmd, host, has_secret, None);
    cmd.process_group(0) // own process group → survives terminal close
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errfile));

    let mut child = cmd.spawn().map_err(|e| {
        let _ = std::fs::remove_file(&log_path);
        format!("could not launch ssh: {e} — is an OpenSSH client on your PATH?")
    })?;
    let pid = child.id() as i32;

    let deadline = Instant::now() + READINESS_GRACE;
    loop {
        match child.try_wait() {
            // Exited before the grace elapsed → the bind or auth failed.
            Ok(Some(status)) => {
                let stderr = std::fs::read_to_string(&log_path).unwrap_or_default();
                let _ = std::fs::remove_file(&log_path);
                return Err(classify_forward_error(
                    &stderr,
                    kind,
                    &spec,
                    status.code(),
                    host,
                    has_secret,
                ));
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&log_path);
                return Err(format!("ssh error: {e}"));
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(POLL);
    }

    // Still alive → up. Drop the Child WITHOUT waiting or killing it (Child::drop does neither on
    // Unix), so the forward keeps running and is reparented to init when sshelf exits.
    drop(child);
    Ok(ForwardEntry {
        id,
        host_id: host.id.clone(),
        host_name: host_name.to_string(),
        kind,
        display: spec.display_string(kind),
        spec,
        pid,
        started_at: now_unix(),
    })
}

/// Ask the OS for a pid's process state + command line. `None` if the pid is gone. Uses `-ww` so
/// the command isn't width-truncated (Linux), and reads `state` first so a zombie is detectable.
fn ps_state_command(pid: i32) -> Option<(String, String)> {
    let out = Command::new("ps")
        .args(["-ww", "-o", "state=,command=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.trim();
    if line.is_empty() {
        return None;
    }
    match line.split_once(char::is_whitespace) {
        Some((state, command)) => Some((state.trim().to_string(), command.trim().to_string())),
        None => Some((line.to_string(), String::new())),
    }
}

/// Decide, from a pid's `ps` state + command, whether it is still *our* live forward. A zombie
/// (state `Z`) is dead; a command that no longer matches our `ssh … <spec>` means the pid was
/// recycled (PID reuse) — also "not ours".
fn parse_ps_alive(state: &str, command: &str, spec_token: &str) -> bool {
    !state.starts_with('Z') && command.contains("ssh") && command.contains(spec_token)
}

/// Whether a recorded forward's process is still alive and still ours.
fn is_alive(entry: &ForwardEntry) -> bool {
    match ps_state_command(entry.pid) {
        Some((state, command)) => parse_ps_alive(&state, &command, &entry.spec_token()),
        None => false,
    }
}

/// Stop a forward: `SIGTERM`, then `SIGKILL` if it lingers — but only signal the pid while it is
/// verifiably still ours (so a recycled pid is never signalled). Also removes its stderr log.
pub fn kill(entry: &ForwardEntry) {
    if is_alive(entry) {
        let _ = signal(entry.pid, "TERM");
        let deadline = Instant::now() + KILL_GRACE;
        while Instant::now() < deadline {
            if !is_alive(entry) {
                break;
            }
            std::thread::sleep(POLL);
        }
        if is_alive(entry) {
            let _ = signal(entry.pid, "KILL");
        }
    }
    remove_log(&entry.id);
}

fn signal(pid: i32, sig: &str) -> std::io::Result<()> {
    Command::new("kill")
        .args([&format!("-{sig}"), &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
}

/// Drop every recorded forward whose process is gone (dead, zombie, or a recycled pid), returning
/// the dropped entries (for a status line). Cleans up the stderr log of each removed forward.
pub fn reconcile(state: &mut ForwardsState) -> Vec<ForwardEntry> {
    let mut dropped = Vec::new();
    state.forwards.retain(|e| {
        if is_alive(e) {
            true
        } else {
            // A missing log is fine — the file may already be gone. A forward recorded by an
            // older build logged to `/tmp`, which `remove_log` reaps too.
            remove_log(&e.id);
            dropped.push(e.clone());
            false
        }
    });
    dropped
}

/// Map a forward's failure (its full stderr + exit code) to a friendly, actionable message. The
/// whole stderr is scanned because the useful line ("Address already in use") is often not the
/// last one — ssh appends a generic "Could not request local forwarding." after it.
fn classify_forward_error(
    stderr: &str,
    kind: ForwardKind,
    spec: &ForwardSpec,
    code: Option<i32>,
    host: &Host,
    has_secret: bool,
) -> String {
    let low = stderr.to_lowercase();
    let port = spec.listen_port;
    let where_ = match kind {
        ForwardKind::Remote => "remote",
        _ => "local",
    };

    if low.contains("address already in use") || low.contains("cannot listen to port") {
        return format!("the {where_} port {port} is already in use — pick another");
    }
    if low.contains("privileged ports")
        || (low.contains("permission denied") && low.contains("bind"))
    {
        return format!(
            "port {port} is privileged (below 1024) — use a port ≥ 1024 or run as root"
        );
    }
    if low.contains("remote port forwarding failed") {
        return "the server refused the remote forward (check its sshd GatewayPorts setting)"
            .into();
    }
    if low.contains("could not resolve")
        || low.contains("name or service not known")
        || low.contains("nodename nor servname")
    {
        return "a hostname could not be resolved — check the host's address and the forward's \
                target"
            .into();
    }
    if low.contains("connection refused") {
        return "connection refused — check the host is up and listening on its ssh port".into();
    }
    if low.contains("timed out") || low.contains("timeout") {
        return "connection timed out — check the host is reachable from here".into();
    }
    if let Some(msg) = ssh::classify_auth_error(stderr, host, has_secret) {
        return msg;
    }
    if low.contains("permission denied")
        || low.contains("authentication failed")
        || low.contains("too many authentication failures")
    {
        return "authentication failed — check the host's key, your agent, or its stored password"
            .into();
    }
    // Unknown failure: show the most useful (last non-blank) line, else the exit code.
    if let Some(line) = tidy_error(stderr) {
        return line;
    }
    match code {
        Some(c) => format!("ssh exited (code {c}) before the forward came up"),
        None => "ssh exited before the forward came up".into(),
    }
}

/// The last non-blank line of `raw` (ssh puts the real cause last), if any.
fn tidy_error(raw: &str) -> Option<String> {
    raw.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AuthMethod;

    /// An agent host, which is the shape that reaches the classifier with nothing stored.
    fn a_host() -> Host {
        let mut h = Host::new("web", "10.0.0.1");
        h.user = Some("deploy".into());
        h
    }

    fn local(listen: u16, host: &str, target: u16) -> ForwardSpec {
        ForwardSpec {
            bind: None,
            listen_port: listen,
            target_host: Some(host.into()),
            target_port: Some(target),
        }
    }

    #[test]
    fn local_args_apply_bind_and_host_defaults() {
        let s = local(8080, "db", 3306);
        assert_eq!(
            forward_args(ForwardKind::Local, &s),
            vec!["-L", "127.0.0.1:8080:db:3306"]
        );
    }

    #[test]
    fn explicit_bind_and_empty_host_default() {
        let s = ForwardSpec {
            bind: Some("0.0.0.0".into()),
            listen_port: 9090,
            target_host: None, // → localhost
            target_port: Some(3000),
        };
        assert_eq!(
            forward_args(ForwardKind::Remote, &s),
            vec!["-R", "0.0.0.0:9090:localhost:3000"]
        );
    }

    #[test]
    fn dynamic_args_are_bind_and_port_only() {
        let s = ForwardSpec {
            bind: None,
            listen_port: 1080,
            target_host: None,
            target_port: None,
        };
        assert_eq!(
            forward_args(ForwardKind::Dynamic, &s),
            vec!["-D", "127.0.0.1:1080"]
        );
    }

    #[test]
    fn build_command_prepends_exit_on_forward_failure() {
        let mut h = Host::new("web", "10.0.0.1");
        h.user = Some("deploy".into());
        let argv = build_forward_command(&h, ForwardKind::Local, &local(8080, "db", 3306), false);
        assert_eq!(argv[0], "-N");
        assert_eq!(&argv[1..3], &["-o", "ExitOnForwardFailure=yes"]);
        assert!(
            argv.windows(2)
                .any(|w| w == ["-L", "127.0.0.1:8080:db:3306"])
        );
        assert_eq!(argv.last().unwrap(), "deploy@10.0.0.1");
    }

    #[test]
    fn display_strings_read_well() {
        assert_eq!(
            local(8080, "db", 3306).display_string(ForwardKind::Local),
            "L  127.0.0.1:8080 → db:3306"
        );
        let dyn_spec = ForwardSpec {
            bind: None,
            listen_port: 1080,
            target_host: None,
            target_port: None,
        };
        assert_eq!(
            dyn_spec.display_string(ForwardKind::Dynamic),
            "D  127.0.0.1:1080 (SOCKS)"
        );
    }

    #[test]
    fn validate_rejects_zero_ports_and_missing_target() {
        assert!(local(0, "db", 3306).validate(ForwardKind::Local).is_err());
        assert!(local(8080, "db", 0).validate(ForwardKind::Local).is_err());
        let no_target = ForwardSpec {
            bind: None,
            listen_port: 8080,
            target_host: None,
            target_port: None,
        };
        assert!(no_target.validate(ForwardKind::Local).is_err());
        // Dynamic needs only the listen port.
        assert!(no_target.validate(ForwardKind::Dynamic).is_ok());
    }

    /// Issue #18, the forward half: a detached forward has no terminal of its own either, and
    /// its stderr goes to a log file — so a passphrase prompt landed on the TUI and the forward
    /// was then reported "up" while ssh sat behind it.
    #[test]
    fn a_forward_with_no_secret_to_supply_can_never_stop_on_a_prompt() {
        let s = local(8080, "db", 3306);
        let a = build_forward_command(&a_host(), ForwardKind::Local, &s, false);
        assert!(
            a.windows(2).any(|w| w == ["-o", "BatchMode=yes"]),
            "a detached forward must not be able to prompt: {a:?}"
        );
        let wired = build_forward_command(&a_host(), ForwardKind::Local, &s, true);
        assert!(!wired.iter().any(|x| x == "BatchMode=yes"));
    }

    #[test]
    fn classify_maps_known_stderr() {
        let s = local(8080, "db", 3306);
        // A stored secret keeps these on the branches they were written for — the auth-guidance
        // branch below only fires when sshelf had nothing to supply.
        let c = |stderr: &str, kind| {
            classify_forward_error(stderr, kind, &s, Some(255), &a_host(), true)
        };

        // The real multi-line stderr: the useful line is NOT last (ssh appends a generic line).
        let busy = "bind [127.0.0.1]:8080: Address already in use\n\
                    channel_setup_fwd_listener_tcpip: cannot listen to port: 8080\n\
                    Could not request local forwarding.";
        assert!(c(busy, ForwardKind::Local).contains("already in use"));

        let privileged =
            "bind [127.0.0.1]:80: Permission denied\nCould not request local forwarding.";
        assert!(c(privileged, ForwardKind::Local).contains("privileged"));

        assert!(
            c(
                "Warning: remote port forwarding failed for listen port 80",
                ForwardKind::Remote
            )
            .contains("server refused")
        );
        assert!(c("ssh: Could not resolve hostname nope", ForwardKind::Local).contains("resolve"));
        assert!(
            c(
                "Permission denied (publickey,password).",
                ForwardKind::Local
            )
            .contains("authentication")
        );

        // Unknown line falls back to the line itself; empty falls back to the exit code.
        assert_eq!(
            classify_forward_error(
                "weird ssh message",
                ForwardKind::Local,
                &s,
                Some(7),
                &a_host(),
                true
            ),
            "weird ssh message"
        );
        assert!(
            classify_forward_error("", ForwardKind::Local, &s, Some(7), &a_host(), true)
                .contains("code 7")
        );
    }

    /// Issue #18: with nothing stored the forward runs under `BatchMode=yes`, so a key that
    /// needs a passphrase fails as a bare "Permission denied" rather than parking on a prompt
    /// painted over the TUI. The message has to name the two ways out, or the user is told the
    /// auth failed with no idea why.
    #[test]
    fn an_auth_failure_with_no_stored_secret_says_what_to_do() {
        let s = local(8080, "db", 3306);
        let denied = "Permission denied (publickey).";

        let mut key_host = a_host();
        key_host.auth = AuthMethod::Key;
        key_host.identity_files = vec!["~/.ssh/id_ed25519".into()];
        let msg =
            classify_forward_error(denied, ForwardKind::Local, &s, Some(255), &key_host, false);
        assert!(msg.contains("ssh-add"), "should point at the agent: {msg}");
        assert!(msg.contains("passphrase"), "should name the cause: {msg}");

        let mut pw_host = a_host();
        pw_host.auth = AuthMethod::Password;
        let msg =
            classify_forward_error(denied, ForwardKind::Local, &s, Some(255), &pw_host, false);
        assert!(msg.contains("password is stored"), "{msg}");

        // A bind failure is not an auth failure, even though ssh words it "Permission denied".
        let privileged =
            "bind [127.0.0.1]:80: Permission denied\nCould not request local forwarding.";
        assert!(
            classify_forward_error(
                privileged,
                ForwardKind::Local,
                &s,
                Some(255),
                &key_host,
                false
            )
            .contains("privileged"),
            "the privileged-port branch has to stay ahead of the auth branch"
        );

        // With a secret stored, the generic auth message stands.
        assert!(
            classify_forward_error(denied, ForwardKind::Local, &s, Some(255), &key_host, true)
                .contains("authentication failed")
        );
    }

    #[test]
    fn ps_alive_filters_zombie_and_pid_reuse() {
        let token = "127.0.0.1:8080:db:3306";
        let cmd = format!("/usr/bin/ssh -N -o ExitOnForwardFailure=yes -L {token} deploy@10.0.0.1");
        // Live, ours.
        assert!(parse_ps_alive("S", &cmd, token));
        assert!(parse_ps_alive("Ss", &cmd, token));
        // Zombie → dead even though the command still matches.
        assert!(!parse_ps_alive("Z", &cmd, token));
        // PID reused by something else → not ours.
        assert!(!parse_ps_alive("S", "/usr/bin/vim notes.txt", token));
        // A different forward's command (different spec) → not ours.
        assert!(!parse_ps_alive(
            "S",
            "/usr/bin/ssh -N -L 127.0.0.1:9999:x:1 a@b",
            token
        ));
    }

    #[test]
    fn forward_logs_live_under_the_data_dir() {
        // Built from an explicit data dir rather than `Paths::resolve()`: nothing here reads the
        // process environment, which other tests in this binary are busy setting.
        assert_eq!(
            log_path_in(Path::new("/data/sshelf"), "01ABC"),
            Path::new("/data/sshelf/logs/fwd-01ABC.log")
        );
    }

    #[test]
    fn forward_log_is_private_inside_a_private_directory() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("sshelf-fwd-log-{}", ulid::Ulid::new()));
        let logs = root.join("logs");
        let path = logs.join("fwd-01ABC.log");
        drop(create_log_file(&path).expect("log file should be created"));

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600, "expected 0600, got {:o}", mode(&path));
        assert_eq!(mode(&logs), 0o700, "expected 0700, got {:o}", mode(&logs));
        // A name that is already taken fails rather than being reused or followed.
        assert!(create_log_file(&path).is_err());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn forwards_state_round_trips_and_missing_is_default() {
        let dir = std::env::temp_dir().join(format!("sshelf-fwd-test-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("forwards.json");
        assert!(ForwardsState::load(&path).unwrap().forwards.is_empty());

        let state = ForwardsState {
            forwards: vec![ForwardEntry {
                id: "01ABC".into(),
                host_id: "01HOST".into(),
                host_name: "web".into(),
                kind: ForwardKind::Local,
                spec: local(8080, "db", 3306),
                display: "L  127.0.0.1:8080 → db:3306".into(),
                pid: 4242,
                started_at: 1_700_000_000,
            }],
        };
        state.save(&path).unwrap();
        let loaded = ForwardsState::load(&path).unwrap();
        assert_eq!(loaded.forwards, state.forwards);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore = "spawns a real sshd + ssh; run with `cargo test -- --ignored`"]
    fn forward_binds_tunnels_and_kills() {
        use std::io::Read;
        use std::net::{TcpListener, TcpStream};

        let Some(sshd) = crate::testsupport::start_sshd() else {
            eprintln!("skipping e2e: no usable sshd on this host");
            return;
        };
        let host = crate::testsupport::host_for(&sshd);

        // Grab a free local port (bind to 0, read it, release it) to listen on.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let local_port = listener.local_addr().unwrap().port();
        drop(listener);

        // Local forward: local_port → 127.0.0.1:<sshd port> (reachable from the server == us).
        let spec = ForwardSpec {
            bind: None,
            listen_port: local_port,
            target_host: Some("127.0.0.1".into()),
            target_port: Some(sshd.port),
        };
        let entry = spawn_forward(&host, "e2e", false, ForwardKind::Local, spec.clone())
            .expect("forward should come up");
        assert!(
            is_alive(&entry),
            "forward should be alive right after spawn"
        );

        // Traffic flows: connecting the local port reaches the forwarded sshd banner.
        let mut stream = TcpStream::connect(("127.0.0.1", local_port)).expect("connect to forward");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).expect("read forwarded banner");
        assert_eq!(&buf, b"SSH-", "expected the forwarded SSH banner");
        drop(stream);

        // ExitOnForwardFailure: a second forward on the same port fails with a clear message.
        let err = spawn_forward(&host, "e2e", false, ForwardKind::Local, spec)
            .expect_err("second bind on the same port must fail");
        assert!(err.contains("already in use"), "unexpected error: {err}");

        // Kill the first forward; it dies and reconcile then drops it from a ledger.
        kill(&entry);
        assert!(!is_alive(&entry), "forward should be gone after kill");
        let mut state = ForwardsState {
            forwards: vec![entry.clone()],
        };
        assert_eq!(reconcile(&mut state).len(), 1);
        assert!(state.forwards.is_empty());
    }
}
