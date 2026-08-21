//! Building the `ssh` argv and performing the `exec()` handoff.
//!
//! On connect, the TUI is restored first (by the caller) and then this process is *replaced*
//! by `ssh` via `exec()`, giving ssh the real TTY. Nothing runs after a successful exec, so
//! the caller persists frecency state beforehand.

use crate::config::Tmux;
use crate::model::{AuthMethod, Host};

/// Expand a leading `~` / `~/` to `$HOME`. On the command line the shell normally does this,
/// but we `exec` ssh directly (no shell), so we must expand identity-file paths ourselves.
fn expand_tilde(path: &str) -> String {
    if path == "~"
        && let Ok(home) = std::env::var("HOME")
    {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

/// Build the argument vector passed to `ssh` (excluding the program name).
///
/// `expand`: expand identity-file `~` so the generated argv stays valid without relying on a
/// shell. `command_string` also expands before quoting, because quoted `~` is not shell-expanded.
pub fn build_args(host: &Host, expand: bool) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();

    if host.auth == AuthMethod::Key {
        for key in &host.identity_files {
            a.push("-i".to_string());
            a.push(if expand {
                expand_tilde(key)
            } else {
                key.clone()
            });
        }
    }

    if let Some(port) = host.port
        && port != 22
    {
        a.push("-p".to_string());
        a.push(port.to_string());
    }

    if !host.jump_hosts.is_empty() {
        a.push("-J".to_string());
        a.push(host.jump_hosts.join(","));
    }

    // Keep the first-connect host-key prompt away from our askpass helper (see ssh-command.md
    // — proven necessary by the M0 spike). Known hosts are still verified; changed keys fail.
    a.push("-o".to_string());
    a.push("StrictHostKeyChecking=accept-new".to_string());

    if let Some(extra) = &host.extra_args
        && let Some(parts) = shlex::split(extra)
    {
        a.extend(parts);
    }

    a.push(format!("{}@{}", host.effective_user(), host.hostname));
    a
}

/// A copy-pasteable `ssh …` command string (identity-file `~` expanded, args shell-quoted).
pub fn command_string(host: &Host) -> String {
    let args = build_args(host, true);
    let joined =
        shlex::try_join(args.iter().map(|s| s.as_str())).unwrap_or_else(|_| args.join(" "));
    format!("ssh {joined}")
}

/// Replace the current process with `ssh`. On success this never returns; it returns an
/// error only if the exec itself fails (e.g. `ssh` not found). The caller must have already
/// restored the terminal.
#[cfg(unix)]
pub fn exec_connect(host: &Host, wire_askpass: bool, two_fa_code: Option<&str>) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let args = build_args(host, true);
    let mut cmd = std::process::Command::new("ssh");
    cmd.args(&args);
    configure_askpass(&mut cmd, host, wire_askpass, two_fa_code);
    // exec() returns only on failure.
    anyhow::anyhow!("failed to launch ssh: {}", cmd.exec())
}

#[cfg(not(unix))]
pub fn exec_connect(host: &Host, wire_askpass: bool, two_fa_code: Option<&str>) -> anyhow::Error {
    // No process-replacement on non-unix; spawn + wait, then mirror the exit code.
    let args = build_args(host, true);
    let mut cmd = std::process::Command::new("ssh");
    cmd.args(&args);
    configure_askpass(&mut cmd, host, wire_askpass, two_fa_code);
    match cmd.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => anyhow::anyhow!("failed to launch ssh: {e}"),
    }
}

/// Why a connection cannot be handed to tmux and must `exec()` in place instead. Each variant
/// carries its own explanation, shown to the user before the handoff (see `docs/search-connect.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmuxFallback {
    /// A one-time 2FA code has to reach `ssh`, and the only way across the tmux boundary is
    /// `new-window -e KEY=VAL` — i.e. the tmux client's argv, readable by anyone with `ps`.
    TwoFactor,
    /// Vault mode: the askpass helper unlocks `vault.age` with `$SSHELF_VAULT_PASSPHRASE`, which
    /// would have to cross the same argv boundary. Same threat, same answer.
    VaultPassphrase,
    /// This tmux predates `-e` on `new-window`/`split-window` (3.0), so the askpass wiring a
    /// stored-secret host needs cannot be handed to the new window at all.
    TmuxTooOld,
}

impl TmuxFallback {
    /// One line, shown just before the in-place connect so the missing tmux window isn't a
    /// mystery.
    pub fn message(self) -> &'static str {
        match self {
            TmuxFallback::TwoFactor => {
                "2FA host — connecting here (a verification code would ride tmux's argv)"
            }
            TmuxFallback::VaultPassphrase => {
                "vault-mode password host — connecting here (the passphrase would ride tmux's argv)"
            }
            TmuxFallback::TmuxTooOld => {
                "tmux is older than 3.0 — connecting here (it can't carry the askpass wiring)"
            }
        }
    }
}

/// Whether this process is running inside tmux (`$TMUX` is set by the server for its panes).
pub fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|v| !v.is_empty())
}

/// Decide whether a connection can be opened in tmux, given what it needs to authenticate.
///
/// `wire_askpass` = a secret is stored for this host, `two_fa_code` = a code was collected in the
/// TUI. Returns `Err(reason)` when the connection must `exec()` in place instead; see D-025.
pub fn tmux_fallback(wire_askpass: bool, has_2fa_code: bool) -> Result<(), TmuxFallback> {
    if has_2fa_code {
        return Err(TmuxFallback::TwoFactor);
    }
    // Only a wired askpass ever reads the vault; a key/agent host needs no env at all.
    if wire_askpass
        && std::env::var_os(crate::secrets::VAULT_PASS_ENV).is_some_and(|v| !v.is_empty())
    {
        return Err(TmuxFallback::VaultPassphrase);
    }
    if wire_askpass && !tmux_supports_env() {
        return Err(TmuxFallback::TmuxTooOld);
    }
    Ok(())
}

/// True when the user's tmux understands `-e` on `new-window`/`split-window` (added in 3.0).
/// An unreadable or unparseable `tmux -V` is treated as too old — falling back to `exec()` is
/// always correct, just less convenient.
fn tmux_supports_env() -> bool {
    let out = std::process::Command::new("tmux").arg("-V").output();
    match out {
        Ok(o) if o.status.success() => tmux_version_at_least_3(&String::from_utf8_lossy(&o.stdout)),
        _ => false,
    }
}

/// Parse `tmux -V` output (`tmux 3.4`, `tmux 3.2a`, `tmux next-3.6`, `tmux master`) and report
/// whether it is at least 3.0. `master`/`next-*` are treated as new enough.
fn tmux_version_at_least_3(output: &str) -> bool {
    let Some(raw) = output.split_whitespace().nth(1) else {
        return false;
    };
    if raw == "master" {
        return true;
    }
    let raw = raw.strip_prefix("next-").unwrap_or(raw);
    let major: String = raw.chars().take_while(char::is_ascii_digit).collect();
    major.parse::<u32>().is_ok_and(|m| m >= 3)
}

/// The environment pairs that must cross into a tmux pane for `host` to authenticate exactly as
/// an in-place connect would.
///
/// **These land in the tmux client's argv** (`new-window -e KEY=VAL`), so every value here is
/// public: `SSHELF_HOST_ID` is an opaque id the helper trades for the real secret, and the rest is
/// plumbing. The stored secret, the 2FA code (`SSHELF_2FA_CODE`) and the vault passphrase are
/// **never** included — a connection that would need one falls back to `exec()`
/// ([`tmux_fallback`]). Returns nothing for key/agent hosts: they need no wiring at all.
pub fn tmux_env(host: &Host, wire_askpass: bool) -> Vec<(String, String)> {
    if !wire_askpass {
        return Vec::new();
    }
    let Ok(exe) = std::env::current_exe() else {
        return Vec::new();
    };
    vec![
        ("SSH_ASKPASS".to_string(), exe.display().to_string()),
        ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
        ("SSHELF_ASKPASS".to_string(), "1".to_string()),
        ("SSHELF_HOST_ID".to_string(), host.id.clone()),
    ]
}

/// A tmux window name for `host`: printable characters only, no whitespace, capped in length.
/// tmux shows this in the status line, so a hostile or empty name can't be allowed through.
fn window_name(host: &Host) -> String {
    let cleaned: String = host
        .name
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| !c.is_control())
        .take(32)
        .collect();
    if cleaned.trim_matches('-').is_empty() {
        "sshelf".to_string()
    } else {
        cleaned
    }
}

/// The full `tmux` argv (program name excluded) that opens `host` in a new window or pane.
///
/// `mode` must not be [`Tmux::Off`] — the caller decides that before getting here. The ssh argv is
/// passed as separate arguments, not one string, so tmux `execvp`s it directly and no shell
/// re-parses paths with spaces. `-n` names the window (`split-window` has no such flag — a pane
/// lives in its parent's window).
pub fn tmux_connect_args(mode: Tmux, host: &Host, env: &[(String, String)]) -> Vec<String> {
    let mut a = vec![mode.command().unwrap_or("new-window").to_string()];
    for (key, value) in env {
        a.push("-e".to_string());
        a.push(format!("{key}={value}"));
    }
    if mode == Tmux::Window {
        a.push("-n".to_string());
        a.push(window_name(host));
    }
    a.push("--".to_string());
    a.push("ssh".to_string());
    a.extend(build_args(host, true));
    a
}

/// Open `host` in a new tmux window/pane and return its name for the status line. sshelf keeps
/// running — that's the point of the mode. The caller has already persisted frecency (the tmux
/// spawn is the point of no return for this connection, exactly as `exec()` is).
pub fn tmux_connect(mode: Tmux, host: &Host, wire_askpass: bool) -> Result<String, String> {
    let env = tmux_env(host, wire_askpass);
    let args = tmux_connect_args(mode, host, &env);
    let out = std::process::Command::new("tmux")
        .args(&args)
        .output()
        .map_err(|e| format!("could not run tmux: {e} — is tmux on your PATH?"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let detail = err
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("tmux reported no reason");
        return Err(format!("tmux could not open a {}: {detail}", mode.noun()));
    }
    Ok(window_name(host))
}

/// Wire our own binary as the `SSH_ASKPASS` helper so the stored secret (a login password OR a
/// key passphrase) and/or a queued one-time 2FA code are supplied automatically. The helper is
/// wired (with `SSH_ASKPASS_REQUIRE=force`) when there's a secret to supply (`wire_askpass`) OR a
/// `two_fa_code` to inject; otherwise any inherited askpass is cleared so ssh prompts / uses the
/// agent normally.
///
/// Reused by the transfer worker + the port-forward spawner to authenticate exactly as connect
/// does (they pass `two_fa_code: None`).
pub(crate) fn configure_askpass(
    cmd: &mut std::process::Command,
    host: &Host,
    wire_askpass: bool,
    two_fa_code: Option<&str>,
) {
    cmd.env_remove("SSH_ASKPASS")
        .env_remove("SSH_ASKPASS_REQUIRE")
        .env_remove(crate::askpass::CODE_ENV);
    if !wire_askpass {
        // No stored secret → the exec'd ssh (and our helper) has no business inheriting the
        // vault master passphrase (it may be exported in the shell for headless use). In the
        // wired case it must stay: the helper runs as ssh's child and reads it to unlock the
        // vault (see docs/ssh-command.md). A 2FA-only wire still scrubs it (no secret lookup).
        cmd.env_remove(crate::secrets::VAULT_PASS_ENV);
    }
    if !wire_askpass && two_fa_code.is_none() {
        return;
    }
    if let Some(code) = two_fa_code {
        cmd.env(crate::askpass::CODE_ENV, code);
    }
    if let Ok(exe) = std::env::current_exe() {
        cmd.env("SSH_ASKPASS", exe)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("SSHELF_ASKPASS", "1")
            .env("SSHELF_HOST_ID", &host.id);
    }
}

/// Best-effort copy to the system clipboard. Returns `true` on success. On Linux the
/// clipboard may not persist after the process exits, so the caller also shows the command.
pub fn copy_to_clipboard(text: &str) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.set_text(text.to_owned()).is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AuthMethod, Host};

    #[test]
    fn key_host_builds_identity_and_endpoint() {
        let mut h = Host::new("web", "10.0.0.1");
        h.user = Some("deploy".into());
        h.auth = AuthMethod::Key;
        h.identity_files = vec!["/abs/key".into()];
        let args = build_args(&h, true);
        assert_eq!(
            args,
            vec![
                "-i",
                "/abs/key",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "deploy@10.0.0.1"
            ]
        );
    }

    #[test]
    fn port_only_when_non_default() {
        let mut h = Host::new("a", "h");
        h.port = Some(22);
        assert!(!build_args(&h, true).contains(&"-p".to_string()));
        h.port = Some(2222);
        let args = build_args(&h, true);
        let p = args.iter().position(|s| s == "-p").unwrap();
        assert_eq!(args[p + 1], "2222");
    }

    #[test]
    fn jump_hosts_are_comma_joined() {
        let mut h = Host::new("a", "target");
        h.jump_hosts = vec!["b1".into(), "b2".into()];
        let args = build_args(&h, true);
        let j = args.iter().position(|s| s == "-J").unwrap();
        assert_eq!(args[j + 1], "b1,b2");
    }

    #[test]
    fn extra_args_are_shlex_split() {
        let mut h = Host::new("a", "h");
        h.extra_args = Some("-o ServerAliveInterval=30 -X".into());
        let args = build_args(&h, true);
        assert!(
            args.windows(2)
                .any(|w| w == ["-o", "ServerAliveInterval=30"])
        );
        assert!(args.contains(&"-X".to_string()));
    }

    #[test]
    fn tilde_expands_only_when_requested() {
        // SAFETY: single-threaded test; sets HOME for the duration.
        unsafe {
            std::env::set_var("HOME", "/home/tester");
        }
        let mut h = Host::new("a", "h");
        h.auth = AuthMethod::Key;
        h.identity_files = vec!["~/.ssh/id".into()];
        assert!(build_args(&h, true).contains(&"/home/tester/.ssh/id".to_string()));
        assert!(build_args(&h, false).contains(&"~/.ssh/id".to_string()));
    }

    #[test]
    fn command_string_is_readable() {
        // SAFETY: single-threaded test; sets HOME for the duration.
        unsafe {
            std::env::set_var("HOME", "/home/tester");
        }
        let mut h = Host::new("a", "example.com");
        h.user = Some("root".into());
        h.auth = AuthMethod::Key;
        h.identity_files = vec!["~/.ssh/id key".into()];
        let s = command_string(&h);
        assert!(s.starts_with("ssh "));
        assert!(s.contains("'/home/tester/.ssh/id key'"));
        assert!(!s.contains("'~"));
        assert!(s.contains("root@example.com"));
    }

    #[test]
    fn vault_env_scrubbed_when_askpass_not_wired() {
        let h = Host::new("a", "h");
        let mut cmd = std::process::Command::new("ssh");
        configure_askpass(&mut cmd, &h, false, None);
        // env_remove shows up as (key, None) in get_envs()
        let scrubbed = cmd
            .get_envs()
            .any(|(k, v)| v.is_none() && k == std::ffi::OsStr::new(crate::secrets::VAULT_PASS_ENV));
        assert!(
            scrubbed,
            "vault passphrase must not leak into a no-askpass ssh"
        );
        // And no askpass is wired.
        assert!(
            !cmd.get_envs()
                .any(|(k, v)| k == std::ffi::OsStr::new("SSHELF_ASKPASS") && v.is_some())
        );
    }

    #[test]
    fn vault_env_kept_when_askpass_wired() {
        let h = Host::new("a", "h");
        let mut cmd = std::process::Command::new("ssh");
        configure_askpass(&mut cmd, &h, true, None);
        // Wired: the helper (ssh's child) needs the env var to unlock the vault.
        let scrubbed = cmd
            .get_envs()
            .any(|(k, v)| v.is_none() && k == std::ffi::OsStr::new(crate::secrets::VAULT_PASS_ENV));
        assert!(!scrubbed);
        let wired = cmd
            .get_envs()
            .any(|(k, v)| k == std::ffi::OsStr::new("SSHELF_ASKPASS") && v.is_some());
        assert!(wired);
    }

    #[test]
    fn tmux_version_gate_accepts_3_and_up() {
        assert!(tmux_version_at_least_3("tmux 3.0\n"));
        assert!(tmux_version_at_least_3("tmux 3.2a\n"));
        assert!(tmux_version_at_least_3("tmux 3.7c\n"));
        assert!(tmux_version_at_least_3("tmux next-3.6\n"));
        assert!(tmux_version_at_least_3("tmux master\n"));
        assert!(!tmux_version_at_least_3("tmux 2.9a\n"));
        assert!(!tmux_version_at_least_3("tmux 1.8\n"));
        // Anything we can't read is treated as too old — falling back is always safe.
        assert!(!tmux_version_at_least_3("tmux\n"));
        assert!(!tmux_version_at_least_3(""));
        assert!(!tmux_version_at_least_3("tmux weird\n"));
    }

    #[test]
    fn tmux_env_is_empty_for_key_and_agent_hosts() {
        let h = Host::new("web", "10.0.0.1");
        assert!(tmux_env(&h, false).is_empty());
    }

    #[test]
    fn tmux_env_carries_only_the_askpass_wiring() {
        let h = Host::new("web", "10.0.0.1");
        let env = tmux_env(&h, true);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "SSH_ASKPASS",
                "SSH_ASKPASS_REQUIRE",
                "SSHELF_ASKPASS",
                "SSHELF_HOST_ID"
            ]
        );
        // The host id is opaque (the helper trades it for the secret) — never the secret itself.
        assert!(env.iter().any(|(k, v)| k == "SSHELF_HOST_ID" && v == &h.id));
    }

    /// The whole point of D-025: `-e KEY=VAL` is the tmux client's argv, visible in `ps`.
    #[test]
    fn no_secret_env_ever_reaches_the_tmux_argv() {
        let mut h = Host::new("legacy", "10.0.0.9");
        h.auth = AuthMethod::Password;
        for wired in [false, true] {
            let argv = tmux_connect_args(Tmux::Window, &h, &tmux_env(&h, wired));
            let joined = argv.join(" ");
            assert!(
                !joined.contains(crate::askpass::CODE_ENV),
                "the 2FA code env must never appear in a tmux argv: {joined}"
            );
            assert!(
                !joined.contains(crate::secrets::VAULT_PASS_ENV),
                "the vault passphrase env must never appear in a tmux argv: {joined}"
            );
        }
    }

    #[test]
    fn tmux_window_argv_names_the_window_and_passes_ssh_argv_verbatim() {
        let mut h = Host::new("prod-web", "10.0.0.1");
        h.user = Some("deploy".into());
        let argv = tmux_connect_args(Tmux::Window, &h, &[]);
        assert_eq!(argv[0], "new-window");
        let n = argv
            .iter()
            .position(|s| s == "-n")
            .expect("names the window");
        assert_eq!(argv[n + 1], "prod-web");
        // `--` ends tmux's own options; the ssh argv follows as separate arguments, so tmux
        // execs it directly instead of letting a shell re-split paths with spaces.
        let sep = argv.iter().position(|s| s == "--").unwrap();
        assert_eq!(argv[sep + 1], "ssh");
        assert_eq!(argv[sep + 2..], build_args(&h, true)[..]);
    }

    #[test]
    fn tmux_pane_argv_splits_and_omits_the_window_name() {
        // `split-window` has no -n: a pane lives inside its parent's window.
        let h = Host::new("web", "10.0.0.1");
        let argv = tmux_connect_args(Tmux::Pane, &h, &[]);
        assert_eq!(argv[0], "split-window");
        assert!(!argv.iter().any(|s| s == "-n"));
    }

    #[test]
    fn tmux_argv_passes_env_as_e_pairs() {
        let mut h = Host::new("legacy", "h");
        h.auth = AuthMethod::Password;
        let env = vec![("SSHELF_ASKPASS".to_string(), "1".to_string())];
        let argv = tmux_connect_args(Tmux::Window, &h, &env);
        assert!(argv.windows(2).any(|w| w == ["-e", "SSHELF_ASKPASS=1"]));
    }

    #[test]
    fn window_names_are_sanitized() {
        let mut h = Host::new("my host", "h");
        assert_eq!(window_name(&h), "my-host");
        h.name = "ev\u{1b}[2Jil".into();
        assert!(!window_name(&h).chars().any(char::is_control));
        h.name = "   ".into();
        assert_eq!(window_name(&h), "sshelf");
        h.name = "x".repeat(80);
        assert_eq!(window_name(&h).chars().count(), 32);
    }

    #[test]
    fn a_queued_2fa_code_always_falls_back_to_exec() {
        assert_eq!(tmux_fallback(false, true), Err(TmuxFallback::TwoFactor));
        assert_eq!(tmux_fallback(true, true), Err(TmuxFallback::TwoFactor));
        assert!(
            TmuxFallback::TwoFactor
                .message()
                .starts_with("2FA host — connecting here")
        );
    }

    #[test]
    fn two_fa_code_wires_askpass_and_sets_code_env() {
        let h = Host::new("a", "h");
        let mut cmd = std::process::Command::new("ssh");
        // No stored secret, but a 2FA code is queued (e.g. a key+2FA host).
        configure_askpass(&mut cmd, &h, false, Some("123456"));
        // The helper is wired so it can answer the verification-code prompt…
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == std::ffi::OsStr::new("SSHELF_ASKPASS") && v.is_some())
        );
        // …the code rides in SSHELF_2FA_CODE…
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == std::ffi::OsStr::new(crate::askpass::CODE_ENV)
                    && v == Some(std::ffi::OsStr::new("123456")))
        );
        // …and with no stored secret the vault passphrase is still scrubbed.
        assert!(
            cmd.get_envs()
                .any(|(k, v)| v.is_none()
                    && k == std::ffi::OsStr::new(crate::secrets::VAULT_PASS_ENV))
        );
    }
}
