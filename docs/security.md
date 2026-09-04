# Security & threat model

`sshelf` stores SSH passwords so it can auto-supply them. This document states exactly what
that protects against and what it does not. The shipped root `SECURITY.md` (M8) is a
user-facing summary of this.

> **Strongly prefer SSH keys / agent over stored passwords.** Password storage exists for
> hosts you can't use keys with; it is the least secure option `sshelf` offers.

## Where secrets live

- The OS keyring, the primary store: macOS Keychain (`security-framework`) or Linux Secret
  Service over D-Bus (`keyring` crate). Service `sshelf`, account = host `id`.
- The `age` vault (opt-in, headless): if `SSHELF_VAULT_PASSPHRASE` is set, secrets go in the
  XDG data dir as `vault.age`, encrypted with that passphrase (`age` passphrase mode = **scrypt**
  KDF + ChaCha20-Poly1305). This is the path for headless Linux with no Secret Service daemon,
  and for automation/CI. v1 reads the passphrase from the env var (deterministic, scriptable);
  an interactive prompt + auto-detection of a missing keyring are future enhancements.
  **Env-inheritance tradeoff:** the askpass helper runs as ssh's *child* and reads the env var
  to decrypt the vault, so for hosts with a stored secret the passphrase is necessarily in the
  ssh process tree's environment (`/proc/<pid>/environ`, same-user readable). For hosts with
  **no** stored secret, `ssh.rs::configure_askpass` strips the variable before the exec.
- Provisioning: `sshelf set-password <name|id>` stores a secret from stdin (so it can be
  piped in headless setups) without going through the TUI.
- **Never** in `hosts.toml`, `state.json`, logs, shell history, or process arguments.

The host-key id is the lookup key in both stores, so renaming a host keeps its secret.

## How the password reaches `ssh`

Via `SSH_ASKPASS`: `ssh` calls the helper, which prints the secret on stdout. The password
is **never** passed as a CLI argument (no `sshpass -p`), so it never appears in `ps`/argv. See
[`ssh-command.md`](./ssh-command.md). The helper matches the *shape* of OpenSSH's standard
prompts (a login password `...password:`, or a key passphrase `Enter passphrase for key ...`)
and declines host-key confirmations, OTP/verification codes, and arbitrary server text, so a
keyboard-interactive server can't phish the stored secret by merely mentioning "password".

## The tmux boundary

With `tmux = "window"`/`"pane"` ([Searching & connecting](search-connect.md#connecting-inside-tmux)),
a connection is opened by a *new tmux window*, which is a child of the tmux **server** and
inherits nothing from sshelf's own process. tmux's only channel is `new-window -e KEY=VALUE`, and
those pairs are the **tmux client's argv**: readable by anyone on the machine with `ps`. That is
exactly the leak `SSH_ASKPASS` exists to avoid, so the rule is content-based:

- Only the askpass *wiring* ever crosses: `SSH_ASKPASS`, `SSH_ASKPASS_REQUIRE=force`,
  `SSHELF_ASKPASS=1`, and `SSHELF_HOST_ID`. The id is an opaque ULID; the helper trades it for
  the secret in the keyring, exactly as it does after an `exec()`. No value there is a secret.
- `SSHELF_2FA_CODE` and `SSHELF_VAULT_PASSPHRASE` never cross. A connection that needs
  either (a 2FA host, or a stored-secret host in vault mode) falls back to the in-place
  `exec()` handoff, where the environment is passed by `fork`/`exec` and never appears in argv.
  A unit test asserts neither variable can appear in a generated tmux argv.
- Key/agent hosts pass no environment at all.
- The ssh argv is handed to tmux as separate arguments after `--`, so tmux `execvp`s it directly
  rather than letting a shell re-parse it.

Rationale and rejected alternatives: [D-025](decisions.md).

## Threat model

### Protected against
- On-disk plaintext exposure: secrets are in the OS keyring or encrypted at rest in the vault.
- Process-listing / argv leakage: the password is delivered via stdin/stdout to `ssh`, not
  argv; the tmux path (above) is held to the same rule, falling back rather than bending it.
- Shell-history leakage: `sshelf` never echoes the command containing a password.
- Casual file snooping: the vault requires the master passphrase (memory-hard KDF).
- `hosts.toml` sharing: it contains no secrets, so it's safe to commit/share/back up.
- Config-file corruption: atomic writes, so a crash mid-write leaves the prior file intact.

### NOT protected against (out of scope)
- A root/admin attacker or malware on the machine, which can read process memory, the keyring,
  or keystrokes. `sshelf` assumes you trust your own machine.
- Keyloggers, which can capture the master passphrase as you type it.
- A compromised OS keyring daemon; sshelf trusts the platform's secret service.
- Physical theft without full-disk encryption. Use FDE, which is an OS-level control.
- Unencrypted backups / cloud sync of `vault.age`. The vault is encrypted, but treat it
  as sensitive and don't rely on it as your only protection in an untrusted backup.

Assumption: `sshelf` targets a developer/operator's own (trusted) machine, not shared or
hostile hosts.

## Operational notes

- No password recovery. Forgetting the vault master passphrase means losing vault secrets.
  Use a passphrase you can recover (e.g. from another password manager).
- macOS unsigned builds: the re-exec'd askpass child reading Keychain can trigger an OS
  approval prompt on each connect (Keychain ACLs are keyed to code signature). Ad-hoc sign dev
  builds; release builds should be signed.
- `StrictHostKeyChecking=accept-new` trusts a *new* host's key on first connect but still
  hard-fails if a *known* host's key changes (MITM protection retained).
- Network: `sshelf` makes no network connections of its own and has no telemetry; it only
  ever launches the OpenSSH tools: `ssh` to connect, and `ssh`/`sftp` for the file-transfer
  screen. Transfers authenticate exactly as connect does (keys/agent, or the stored secret via
  `SSH_ASKPASS`) by opening **one** multiplexed `ssh` ControlMaster and running `sftp` over it,
  so there is no extra secret handling and the secret still never reaches argv. Remote paths are
  quoted for `sftp`'s parser, control characters are stripped from displayed names, and
  `StrictHostKeyChecking=accept-new` applies there too. The optional transfer log
  (`--transfer-log` / `$SSHELF_TRANSFER_LOG`) records the `ssh`/`sftp` commands and their stderr
  for troubleshooting, and it contains **no secrets** (same reason: the password goes via askpass).

## Reporting

(M8) Add a `SECURITY.md` at the repo root with a disclosure contact before public release.
