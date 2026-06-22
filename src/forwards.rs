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
pub fn build_forward_command(host: &Host, kind: ForwardKind, spec: &ForwardSpec) -> Vec<String> {
    let mut a = vec![
        "-N".to_string(),
        "-o".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
    ];
    a.extend(forward_args(kind, spec));
    a.extend(ssh::build_args(host, true));
    a
}

/// Where a forward's stderr is logged (a regular file, so a long-lived `ssh` never gets SIGPIPE
/// from a closed pipe). Derived from the id, so `reconcile`/`kill` can clean it up too.
fn log_path_for(id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("sshelf-fwd-{id}.log"))
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
    let log_path = log_path_for(&id);
    let errfile = std::fs::File::create(&log_path)
        .map_err(|e| format!("could not create forward log {}: {e}", log_path.display()))?;

    let mut cmd = Command::new("ssh");
    cmd.args(build_forward_command(host, kind, &spec));
    ssh::configure_askpass(&mut cmd, host, has_secret);
    cmd.process_group(0) // own process group → survives terminal close
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errfile));

    let mut child = cmd.spawn().map_err(|e| {
        let _ = std::fs::remove_file(&log_path);
        format!("could not launch ssh: {e}")
    })?;
    let pid = child.id() as i32;

    let deadline = Instant::now() + READINESS_GRACE;
    loop {
        match child.try_wait() {
            // Exited before the grace elapsed → the bind or auth failed.
            Ok(Some(status)) => {
                let stderr = std::fs::read_to_string(&log_path).unwrap_or_default();
                let _ = std::fs::remove_file(&log_path);
                return Err(classify_forward_error(&stderr, kind, &spec, status.code()));
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
    let _ = std::fs::remove_file(log_path_for(&entry.id));
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
            let _ = std::fs::remove_file(log_path_for(&e.id));
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
        return "could not resolve the host".into();
    }
    if low.contains("connection refused") {
        return "connection refused".into();
    }
    if low.contains("timed out") || low.contains("timeout") {
        return "connection timed out".into();
    }
    if low.contains("permission denied")
        || low.contains("authentication failed")
        || low.contains("too many authentication failures")
    {
        return "authentication failed".into();
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
        let argv = build_forward_command(&h, ForwardKind::Local, &local(8080, "db", 3306));
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

    #[test]
    fn classify_maps_known_stderr() {
        let s = local(8080, "db", 3306);
        // The real multi-line stderr: the useful line is NOT last (ssh appends a generic line).
        let busy = "bind [127.0.0.1]:8080: Address already in use\n\
                    channel_setup_fwd_listener_tcpip: cannot listen to port: 8080\n\
                    Could not request local forwarding.";
        assert!(
            classify_forward_error(busy, ForwardKind::Local, &s, Some(255))
                .contains("already in use")
        );
        let privileged =
            "bind [127.0.0.1]:80: Permission denied\nCould not request local forwarding.";
        assert!(
            classify_forward_error(privileged, ForwardKind::Local, &s, Some(255))
                .contains("privileged")
        );
        assert!(
            classify_forward_error(
                "Warning: remote port forwarding failed for listen port 80",
                ForwardKind::Remote,
                &s,
                Some(255),
            )
            .contains("server refused")
        );
        assert!(
            classify_forward_error(
                "ssh: Could not resolve hostname nope",
                ForwardKind::Local,
                &s,
                Some(255)
            )
            .contains("resolve")
        );
        assert!(
            classify_forward_error(
                "Permission denied (publickey,password).",
                ForwardKind::Local,
                &s,
                Some(255)
            )
            .contains("authentication")
        );
        // Unknown line falls back to the line itself; empty falls back to the exit code.
        assert_eq!(
            classify_forward_error("weird ssh message", ForwardKind::Local, &s, Some(7)),
            "weird ssh message"
        );
        assert!(classify_forward_error("", ForwardKind::Local, &s, Some(7)).contains("code 7"));
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
