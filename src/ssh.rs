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

/// The one line printed just before the handoff when a jump chain forces the terminal prompt.
pub const MULTI_HOP_NOTICE: &str =
    "multi-hop jump with a stored secret: ssh will ask for it on the terminal";

/// How a connection expresses its ProxyJump chain.
///
/// `ssh` passes `SSH_ASKPASS` down to the process it starts for a `ProxyJump` hop, and it does
/// **not** forward the destination's `-o` options to that hop (only `-l`, `-p`, `-J`, `-F` and
/// `-v` cross over). So a hop can ask for a password and be handed the *target's* stored secret.
/// Nothing on the target's command line prevents it. See D-029.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JumpPlan {
    /// `-J <chain>` exactly as stored, or no jump host at all. Nothing is wired that a hop could
    /// inherit, so there is nothing to protect.
    Direct,
    /// One jump host, and a secret to keep away from it: an explicit `ProxyCommand` that leaves
    /// the hop with only an agent or an unencrypted key file, which is the documented rule.
    Proxy(String),
    /// Two or more hops (or one sshelf will not put inside a shell), and a secret to keep away
    /// from them: `-J` stays, the helper is not wired at all, and `ssh` asks on the terminal.
    Terminal,
}

/// `[user@]host[:port]`, parsed out of a stored jump-host string.
struct JumpTarget<'a> {
    user: Option<&'a str>,
    host: &'a str,
    port: Option<u16>,
}

/// True if `s` is safe to place inside a `ProxyCommand`. OpenSSH runs that value through the
/// user's shell, so the allowlist is deliberately narrow: anything outside it takes the
/// terminal-prompt path instead of being quoted and hoped for.
fn jump_is_safe(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | ':' | '[' | ']' | '-')
        })
}

/// Split a stored jump-host string into its parts, or `None` when it is unsafe or ambiguous.
fn parse_jump(s: &str) -> Option<JumpTarget<'_>> {
    if !jump_is_safe(s) {
        return None;
    }
    // The user is everything before the last `@`; an IPv6 literal has none.
    let (user, rest) = match s.rsplit_once('@') {
        Some((u, r)) if !u.is_empty() && !r.is_empty() => (Some(u), r),
        Some(_) => return None,
        None => (None, s),
    };
    let (host, port) = if let Some(inner) = rest.strip_prefix('[') {
        // `[2001:db8::1]` or `[2001:db8::1]:2222` — the brackets keep the colons unambiguous,
        // and ssh itself wants the address without them.
        let (addr, after) = inner.split_once(']')?;
        match after {
            "" => (addr, None),
            p => (addr, Some(p.strip_prefix(':')?.parse().ok()?)),
        }
    } else {
        match rest.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && !h.contains(':') => (h, Some(p.parse().ok()?)),
            // More than one colon and no brackets: we can't tell an address from a port.
            Some(_) => return None,
            None => (rest, None),
        }
    };
    // A leading `-` in either position would be read by `ssh` as an option rather than as a
    // name. Nothing in the allowlist makes that dangerous on its own, but a hostname is not an
    // option, so refuse it and take the terminal path instead.
    let dashed = host.starts_with('-') || user.is_some_and(|u| u.starts_with('-'));
    (!host.is_empty() && !dashed).then_some(JumpTarget { user, host, port })
}

/// The `ProxyCommand` for one hop. `BatchMode=yes` alone disables password prompts; the two
/// explicit `no`s are there so the guarantee doesn't rest on one reading of the man page.
fn proxy_command(j: &JumpTarget<'_>) -> String {
    let mut c = String::from(
        "ssh -o BatchMode=yes -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no",
    );
    if let Some(user) = j.user {
        c.push_str(&format!(" -l {user}"));
    }
    if let Some(port) = j.port {
        c.push_str(&format!(" -p {port}"));
    }
    c.push_str(&format!(" -W '[%h]:%p' {}", j.host));
    c
}

/// How `host`'s jump chain will be expressed, given whether the askpass helper is wired for
/// this connection (`askpass` = a stored secret **or** a queued verification code).
pub fn jump_plan(host: &Host, askpass: bool) -> JumpPlan {
    if !askpass || host.jump_hosts.is_empty() {
        return JumpPlan::Direct;
    }
    match host.jump_hosts.as_slice() {
        [one] => match parse_jump(one) {
            Some(j) => JumpPlan::Proxy(proxy_command(&j)),
            None => JumpPlan::Terminal,
        },
        _ => JumpPlan::Terminal,
    }
}

/// Build the argument vector passed to `ssh` (excluding the program name).
///
/// `expand`: expand identity-file `~` so the generated argv stays valid without relying on a
/// shell. `command_string` also expands before quoting, because quoted `~` is not shell-expanded.
///
/// `askpass`: the askpass helper will be wired for this run. It decides how the jump chain is
/// expressed ([`jump_plan`]) — a hop must never inherit the helper.
pub fn build_args(host: &Host, expand: bool, askpass: bool) -> Vec<String> {
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

    match jump_plan(host, askpass) {
        JumpPlan::Proxy(cmd) => {
            a.push("-o".to_string());
            a.push(format!("ProxyCommand={cmd}"));
        }
        // Nothing to protect, or a chain we can't constrain: the stored chain, verbatim.
        JumpPlan::Direct | JumpPlan::Terminal => {
            if !host.jump_hosts.is_empty() {
                a.push("-J".to_string());
                a.push(host.jump_hosts.join(","));
            }
        }
    }

    // Keep the first-connect host-key prompt away from our askpass helper (see ssh-command.md
    // — proven necessary by the M0 spike). Known hosts are still verified; changed keys fail.
    a.push("-o".to_string());
    a.push("StrictHostKeyChecking=accept-new".to_string());

    // A key host has no business being steered into a password prompt: without this a server
    // that rejects the key can ask for one over keyboard-interactive, and the helper is right
    // there. 2FA hosts keep keyboard-interactive, which is how the verification code arrives.
    // ssh takes the first value it obtains, so ours wins over anything in `extra_args`.
    if host.auth == AuthMethod::Key {
        a.push("-o".to_string());
        a.push(if host.requires_2fa {
            "PreferredAuthentications=publickey,keyboard-interactive".to_string()
        } else {
            "PreferredAuthentications=publickey".to_string()
        });
    }

    if let Some(extra) = &host.extra_args
        && let Some(parts) = shlex::split(extra)
    {
        a.extend(parts);
    }

    a.push(format!("{}@{}", host.effective_user(), host.hostname));
    a
}

/// A copy-pasteable `ssh …` command string (identity-file `~` expanded, args shell-quoted).
///
/// `askpass` is what the real connect would use, so a host with a stored secret and a jump host
/// shows the same `ProxyCommand` sshelf would run. Callers that must not touch the secret store
/// (`sshelf list --json`, which would otherwise read the keyring once per host) pass `false` and
/// get the stored `-J` chain — which is also the right thing for a command run by hand, since
/// there is no helper for a hop to inherit. See `docs/ssh-command.md`.
pub fn command_string(host: &Host, askpass: bool) -> String {
    let args = build_args(host, true, askpass);
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
    let mut cmd = connect_command(host, wire_askpass, two_fa_code);
    // exec() returns only on failure.
    anyhow::anyhow!(
        "could not launch ssh: {} — is an OpenSSH client installed and on your PATH?",
        cmd.exec()
    )
}

/// The `ssh` command a connect runs: the argv plus whatever askpass wiring the jump plan allows.
///
/// A [`JumpPlan::Terminal`] connection is wired with **nothing** — no helper, no code, no vault
/// passphrase — so `ssh` falls back to asking on the terminal instead of handing the secret to a
/// hop we cannot constrain. The caller prints [`MULTI_HOP_NOTICE`] so that isn't a surprise.
fn connect_command(
    host: &Host,
    wire_askpass: bool,
    two_fa_code: Option<&str>,
) -> std::process::Command {
    let askpass = wire_askpass || two_fa_code.is_some();
    let mut cmd = std::process::Command::new("ssh");
    cmd.args(build_args(host, true, askpass));
    match jump_plan(host, askpass) {
        JumpPlan::Terminal => configure_askpass(&mut cmd, host, false, None),
        _ => configure_askpass(&mut cmd, host, wire_askpass, two_fa_code),
    }
    cmd
}

#[cfg(not(unix))]
pub fn exec_connect(host: &Host, wire_askpass: bool, two_fa_code: Option<&str>) -> anyhow::Error {
    // No process-replacement on non-unix; spawn + wait, then mirror the exit code.
    let mut cmd = connect_command(host, wire_askpass, two_fa_code);
    match cmd.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => anyhow::anyhow!(
            "could not launch ssh: {e} — is an OpenSSH client installed and on your PATH?"
        ),
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
    /// Two or more jump hops with a secret to protect ([`JumpPlan::Terminal`]): there is no
    /// wiring to hand to a new window, and no terminal in one for `ssh` to ask on either.
    MultiHopJump,
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
            TmuxFallback::MultiHopJump => {
                "multi-hop jump host — connecting here (ssh has to ask for the secret on a terminal)"
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
pub fn tmux_fallback(
    host: &Host,
    wire_askpass: bool,
    has_2fa_code: bool,
) -> Result<(), TmuxFallback> {
    if has_2fa_code {
        return Err(TmuxFallback::TwoFactor);
    }
    // A chain we can't constrain has to prompt on a terminal, and a fresh window has none.
    if matches!(jump_plan(host, wire_askpass), JumpPlan::Terminal) {
        return Err(TmuxFallback::MultiHopJump);
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
/// public: `SSHELF_HOST_ID` is an opaque id the helper trades for the real secret, the secret
/// kind and the identity-file list are what scope the helper to its own prompt (D-029), and the
/// rest is plumbing. The stored secret, the 2FA code (`SSHELF_2FA_CODE`) and the vault passphrase
/// are **never** included — a connection that would need one falls back to `exec()`
/// ([`tmux_fallback`]). Returns nothing for key/agent hosts: they need no wiring at all.
pub fn tmux_env(host: &Host, wire_askpass: bool) -> Vec<(String, String)> {
    if !wire_askpass {
        return Vec::new();
    }
    let Ok(exe) = std::env::current_exe() else {
        return Vec::new();
    };
    let mut env = vec![
        ("SSH_ASKPASS".to_string(), exe.display().to_string()),
        ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
        ("SSHELF_ASKPASS".to_string(), "1".to_string()),
        ("SSHELF_HOST_ID".to_string(), host.id.clone()),
        (
            crate::askpass::KIND_ENV.to_string(),
            secret_kind(host).as_str().to_string(),
        ),
    ];
    if host.auth == AuthMethod::Key {
        env.push((
            crate::askpass::IDENTITY_ENV.to_string(),
            identity_list(host),
        ));
    }
    env
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
pub fn tmux_connect_args(
    mode: Tmux,
    host: &Host,
    env: &[(String, String)],
    askpass: bool,
) -> Vec<String> {
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
    a.extend(build_args(host, true, askpass));
    a
}

/// Open `host` in a new tmux window/pane and return its name for the status line. sshelf keeps
/// running — that's the point of the mode. The caller has already persisted frecency (the tmux
/// spawn is the point of no return for this connection, exactly as `exec()` is).
pub fn tmux_connect(mode: Tmux, host: &Host, wire_askpass: bool) -> Result<String, String> {
    let env = tmux_env(host, wire_askpass);
    let args = tmux_connect_args(mode, host, &env, wire_askpass);
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
/// The helper is also told **which** secret it is holding and, for a key host, which key files
/// are in play, so it can answer its own prompt shape and decline everything else (D-029).
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
        .env_remove(crate::askpass::CODE_ENV)
        .env_remove(crate::askpass::KIND_ENV)
        .env_remove(crate::askpass::IDENTITY_ENV);
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
            .env("SSHELF_HOST_ID", &host.id)
            .env(crate::askpass::KIND_ENV, secret_kind(host).as_str());
        if host.auth == AuthMethod::Key {
            cmd.env(crate::askpass::IDENTITY_ENV, identity_list(host));
        }
    }
}

/// Which secret the helper would be holding for `host` — its auth method, in the helper's terms.
fn secret_kind(host: &Host) -> crate::askpass::SecretKind {
    match host.auth {
        AuthMethod::Password => crate::askpass::SecretKind::Password,
        AuthMethod::Key => crate::askpass::SecretKind::Passphrase,
        AuthMethod::Agent => crate::askpass::SecretKind::Agent,
    }
}

/// The host's identity files, `~`-expanded and `:`-separated, exactly as they are passed with
/// `-i`. The helper compares OpenSSH's passphrase prompt against this list, so the two have to
/// be spelled the same way.
fn identity_list(host: &Host) -> String {
    host.identity_files
        .iter()
        .map(|k| expand_tilde(k))
        .collect::<Vec<_>>()
        .join(":")
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
        let args = build_args(&h, true, false);
        assert_eq!(
            args,
            vec![
                "-i",
                "/abs/key",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "PreferredAuthentications=publickey",
                "deploy@10.0.0.1"
            ]
        );
    }

    #[test]
    fn port_only_when_non_default() {
        let mut h = Host::new("a", "h");
        h.port = Some(22);
        assert!(!build_args(&h, true, false).contains(&"-p".to_string()));
        h.port = Some(2222);
        let args = build_args(&h, true, false);
        let p = args.iter().position(|s| s == "-p").unwrap();
        assert_eq!(args[p + 1], "2222");
    }

    #[test]
    fn jump_hosts_are_comma_joined() {
        let mut h = Host::new("a", "target");
        h.jump_hosts = vec!["b1".into(), "b2".into()];
        let args = build_args(&h, true, false);
        let j = args.iter().position(|s| s == "-J").unwrap();
        assert_eq!(args[j + 1], "b1,b2");
    }

    #[test]
    fn extra_args_are_shlex_split() {
        let mut h = Host::new("a", "h");
        h.extra_args = Some("-o ServerAliveInterval=30 -X".into());
        let args = build_args(&h, true, false);
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
        assert!(build_args(&h, true, false).contains(&"/home/tester/.ssh/id".to_string()));
        assert!(build_args(&h, false, false).contains(&"~/.ssh/id".to_string()));
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
        let s = command_string(&h, false);
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
                "SSHELF_HOST_ID",
                "SSHELF_SECRET_KIND"
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
            let argv = tmux_connect_args(Tmux::Window, &h, &tmux_env(&h, wired), wired);
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
        let argv = tmux_connect_args(Tmux::Window, &h, &[], false);
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
        assert_eq!(argv[sep + 2..], build_args(&h, true, false)[..]);
    }

    #[test]
    fn tmux_pane_argv_splits_and_omits_the_window_name() {
        // `split-window` has no -n: a pane lives inside its parent's window.
        let h = Host::new("web", "10.0.0.1");
        let argv = tmux_connect_args(Tmux::Pane, &h, &[], false);
        assert_eq!(argv[0], "split-window");
        assert!(!argv.iter().any(|s| s == "-n"));
    }

    #[test]
    fn tmux_argv_passes_env_as_e_pairs() {
        let mut h = Host::new("legacy", "h");
        h.auth = AuthMethod::Password;
        let env = vec![("SSHELF_ASKPASS".to_string(), "1".to_string())];
        let argv = tmux_connect_args(Tmux::Window, &h, &env, false);
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
        let h = Host::new("a", "h");
        assert_eq!(tmux_fallback(&h, false, true), Err(TmuxFallback::TwoFactor));
        assert_eq!(tmux_fallback(&h, true, true), Err(TmuxFallback::TwoFactor));
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

    // ---- D-029: the jump hop never inherits the askpass helper -------------------------------

    #[test]
    fn key_hosts_pin_public_key_auth() {
        let mut h = Host::new("web", "10.0.0.1");
        h.auth = AuthMethod::Key;
        assert!(
            build_args(&h, true, false)
                .windows(2)
                .any(|w| w == ["-o", "PreferredAuthentications=publickey"])
        );
        // A 2FA host still needs keyboard-interactive: that is how the code arrives.
        h.requires_2fa = true;
        assert!(build_args(&h, true, false).windows(2).any(|w| w
            == [
                "-o",
                "PreferredAuthentications=publickey,keyboard-interactive"
            ]));
        // Password and agent hosts are not constrained.
        for auth in [AuthMethod::Password, AuthMethod::Agent] {
            let mut h = Host::new("a", "h");
            h.auth = auth;
            assert!(
                !build_args(&h, true, false)
                    .iter()
                    .any(|s| s.starts_with("PreferredAuthentications"))
            );
        }
    }

    #[test]
    fn one_jump_with_a_wired_secret_becomes_a_proxy_command() {
        let mut h = Host::new("target", "10.0.0.9");
        h.auth = AuthMethod::Password;
        h.jump_hosts = vec!["deploy@bastion.example.com:2222".into()];
        let args = build_args(&h, true, true);
        let i = args.iter().position(|s| s == "-o").expect("an -o option");
        assert_eq!(
            args[i + 1],
            "ProxyCommand=ssh -o BatchMode=yes -o PasswordAuthentication=no \
             -o KbdInteractiveAuthentication=no -l deploy -p 2222 -W '[%h]:%p' bastion.example.com"
        );
        assert!(!args.iter().any(|s| s == "-J"), "-J must be gone: {args:?}");
    }

    #[test]
    fn a_bare_jump_host_needs_no_user_or_port() {
        let mut h = Host::new("target", "10.0.0.9");
        h.auth = AuthMethod::Password;
        h.jump_hosts = vec!["bastion".into()];
        assert!(
            build_args(&h, true, true).contains(
                &"ProxyCommand=ssh -o BatchMode=yes -o PasswordAuthentication=no \
              -o KbdInteractiveAuthentication=no -W '[%h]:%p' bastion"
                    .replace("              ", "")
                    .to_string()
            )
        );
    }

    #[test]
    fn two_jumps_with_a_wired_secret_keep_the_flag_and_prompt() {
        let mut h = Host::new("target", "10.0.0.9");
        h.auth = AuthMethod::Password;
        h.jump_hosts = vec!["b1".into(), "b2".into()];
        assert_eq!(jump_plan(&h, true), JumpPlan::Terminal);
        let args = build_args(&h, true, true);
        let j = args.iter().position(|s| s == "-J").expect("-J is kept");
        assert_eq!(args[j + 1], "b1,b2");
        assert!(!args.iter().any(|s| s.starts_with("ProxyCommand=")));
    }

    #[test]
    fn a_jump_host_is_untouched_when_nothing_is_wired() {
        // An agent host (or a key host with no stored passphrase) has nothing a hop could take.
        let mut h = Host::new("target", "10.0.0.9");
        h.jump_hosts = vec!["bastion".into()];
        assert_eq!(jump_plan(&h, false), JumpPlan::Direct);
        let args = build_args(&h, true, false);
        let j = args.iter().position(|s| s == "-J").expect("-J is kept");
        assert_eq!(args[j + 1], "bastion");
    }

    #[test]
    fn an_unsafe_or_ambiguous_jump_string_takes_the_terminal_path() {
        let mut h = Host::new("target", "10.0.0.9");
        h.auth = AuthMethod::Password;
        for jump in [
            "bastion; rm -rf /",
            "bastion$(id)",
            "`id`",
            "bastion:notaport",
            "2001:db8::1",
            "@bastion",
            "-A",
        ] {
            h.jump_hosts = vec![jump.to_string()];
            assert_eq!(
                jump_plan(&h, true),
                JumpPlan::Terminal,
                "{jump:?} must not reach a ProxyCommand"
            );
            let args = build_args(&h, true, true);
            assert!(args.iter().any(|s| s == "-J"));
            assert!(!args.iter().any(|s| s.starts_with("ProxyCommand=")));
        }
    }

    #[test]
    fn parse_jump_reads_user_host_and_port() {
        let j = parse_jump("deploy@bastion.example.com:2222").unwrap();
        assert_eq!(
            (j.user, j.host, j.port),
            (Some("deploy"), "bastion.example.com", Some(2222))
        );
        let j = parse_jump("bastion").unwrap();
        assert_eq!((j.user, j.host, j.port), (None, "bastion", None));
        // A bracketed IPv6 literal keeps its colons; ssh wants the address without the brackets.
        let j = parse_jump("[2001:db8::1]:2222").unwrap();
        assert_eq!((j.user, j.host, j.port), (None, "2001:db8::1", Some(2222)));
        assert!(parse_jump("").is_none());
        assert!(parse_jump("host:99999").is_none());
        // Neither part may look like an option to `ssh`.
        assert!(parse_jump("-A").is_none());
        assert!(parse_jump("-oSomething").is_none());
        assert!(parse_jump("-l@host").is_none());
    }

    #[test]
    fn askpass_env_names_the_secret_kind_and_the_key_files() {
        // SAFETY: single-threaded test; sets HOME for the duration.
        unsafe {
            std::env::set_var("HOME", "/home/tester");
        }
        let value = |cmd: &std::process::Command, key: &str| {
            cmd.get_envs()
                .find(|(k, _)| *k == std::ffi::OsStr::new(key))
                .and_then(|(_, v)| v)
                .map(|v| v.to_string_lossy().into_owned())
        };

        let mut h = Host::new("web", "10.0.0.1");
        h.auth = AuthMethod::Key;
        h.identity_files = vec!["~/.ssh/id_ed25519".into(), "/abs/other".into()];
        let mut cmd = std::process::Command::new("ssh");
        configure_askpass(&mut cmd, &h, true, None);
        assert_eq!(
            value(&cmd, crate::askpass::KIND_ENV).as_deref(),
            Some("passphrase")
        );
        assert_eq!(
            value(&cmd, crate::askpass::IDENTITY_ENV).as_deref(),
            Some("/home/tester/.ssh/id_ed25519:/abs/other")
        );

        // A password host names its kind and carries no key list at all.
        let mut h = Host::new("legacy", "10.0.0.2");
        h.auth = AuthMethod::Password;
        let mut cmd = std::process::Command::new("ssh");
        configure_askpass(&mut cmd, &h, true, None);
        assert_eq!(
            value(&cmd, crate::askpass::KIND_ENV).as_deref(),
            Some("password")
        );
        assert_eq!(value(&cmd, crate::askpass::IDENTITY_ENV), None);

        // An agent host with a queued code says so, so the helper answers only the code.
        let h = Host::new("vpn", "10.0.0.3");
        let mut cmd = std::process::Command::new("ssh");
        configure_askpass(&mut cmd, &h, false, Some("123456"));
        assert_eq!(
            value(&cmd, crate::askpass::KIND_ENV).as_deref(),
            Some("agent")
        );
    }

    #[test]
    fn a_multi_hop_connect_wires_nothing_at_all() {
        let mut h = Host::new("target", "10.0.0.9");
        h.auth = AuthMethod::Password;
        h.jump_hosts = vec!["b1".into(), "b2".into()];
        let cmd = connect_command(&h, true, None);
        // No helper, no host id, and the vault passphrase is scrubbed: ssh asks on the terminal.
        for key in ["SSH_ASKPASS", "SSHELF_ASKPASS", "SSHELF_HOST_ID"] {
            assert!(
                !cmd.get_envs()
                    .any(|(k, v)| k == std::ffi::OsStr::new(key) && v.is_some()),
                "{key} must not be wired for a multi-hop connect"
            );
        }
        assert!(
            cmd.get_envs()
                .any(|(k, v)| v.is_none()
                    && k == std::ffi::OsStr::new(crate::secrets::VAULT_PASS_ENV))
        );

        // One hop is constrained instead, so the helper stays wired.
        h.jump_hosts = vec!["bastion".into()];
        let cmd = connect_command(&h, true, None);
        assert!(
            cmd.get_envs()
                .any(|(k, v)| k == std::ffi::OsStr::new("SSHELF_ASKPASS") && v.is_some())
        );
    }

    #[test]
    fn tmux_steps_aside_for_a_multi_hop_jump() {
        let mut h = Host::new("target", "10.0.0.9");
        h.auth = AuthMethod::Password;
        h.jump_hosts = vec!["b1".into(), "b2".into()];
        assert_eq!(
            tmux_fallback(&h, true, false),
            Err(TmuxFallback::MultiHopJump)
        );
        // With nothing stored there is nothing to protect, so tmux is fine again.
        assert!(!matches!(
            tmux_fallback(&h, false, false),
            Err(TmuxFallback::MultiHopJump)
        ));
    }

    #[test]
    fn a_tmux_window_for_a_key_host_carries_the_key_list() {
        // SAFETY: single-threaded test; sets HOME for the duration.
        unsafe {
            std::env::set_var("HOME", "/home/tester");
        }
        let mut h = Host::new("web", "10.0.0.1");
        h.auth = AuthMethod::Key;
        h.identity_files = vec!["/abs/key".into()];
        let env = tmux_env(&h, true);
        assert!(
            env.iter()
                .any(|(k, v)| k == "SSHELF_SECRET_KIND" && v == "passphrase")
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "SSHELF_IDENTITY_FILES" && v == "/abs/key")
        );
    }
}
