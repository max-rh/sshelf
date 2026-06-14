//! Building the `ssh` argv and performing the `exec()` handoff.
//!
//! On connect, the TUI is restored first (by the caller) and then this process is *replaced*
//! by `ssh` via `exec()`, giving ssh the real TTY. Nothing runs after a successful exec, so
//! the caller persists frecency state beforehand.

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
pub fn exec_connect(host: &Host, wire_askpass: bool) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let args = build_args(host, true);
    let mut cmd = std::process::Command::new("ssh");
    cmd.args(&args);
    configure_askpass(&mut cmd, host, wire_askpass);
    // exec() returns only on failure.
    anyhow::anyhow!("failed to launch ssh: {}", cmd.exec())
}

#[cfg(not(unix))]
pub fn exec_connect(host: &Host, wire_askpass: bool) -> anyhow::Error {
    // No process-replacement on non-unix; spawn + wait, then mirror the exit code.
    let args = build_args(host, true);
    let mut cmd = std::process::Command::new("ssh");
    cmd.args(&args);
    configure_askpass(&mut cmd, host, wire_askpass);
    match cmd.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => anyhow::anyhow!("failed to launch ssh: {e}"),
    }
}

/// Wire our own binary as the `SSH_ASKPASS` helper so the stored secret (a login password OR
/// a key passphrase) is supplied automatically. Only when `wire_askpass` is set (a secret
/// exists); otherwise clear any inherited askpass so ssh prompts / uses the agent normally.
fn configure_askpass(cmd: &mut std::process::Command, host: &Host, wire_askpass: bool) {
    cmd.env_remove("SSH_ASKPASS")
        .env_remove("SSH_ASKPASS_REQUIRE");
    if !wire_askpass {
        // No stored secret → the askpass helper never runs, so the exec'd ssh has no
        // business inheriting the vault master passphrase (it may be exported in the
        // shell for headless use). In the wired case it must stay: the helper runs as
        // ssh's child and reads it to unlock the vault (see docs/ssh-command.md).
        cmd.env_remove(crate::secrets::VAULT_PASS_ENV);
        return;
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
        configure_askpass(&mut cmd, &h, false);
        // env_remove shows up as (key, None) in get_envs()
        let scrubbed = cmd
            .get_envs()
            .any(|(k, v)| v.is_none() && k == std::ffi::OsStr::new(crate::secrets::VAULT_PASS_ENV));
        assert!(
            scrubbed,
            "vault passphrase must not leak into a no-askpass ssh"
        );
    }

    #[test]
    fn vault_env_kept_when_askpass_wired() {
        let h = Host::new("a", "h");
        let mut cmd = std::process::Command::new("ssh");
        configure_askpass(&mut cmd, &h, true);
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
}
