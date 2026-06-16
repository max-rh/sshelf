# Security & threat model

`sshelf` stores SSH passwords so it can auto-supply them. This document states exactly what
that protects against and what it does not. The shipped root `SECURITY.md` (M8) is a
user-facing summary of this.

> **Strongly prefer SSH keys / agent over stored passwords.** Password storage exists for
> hosts you can't use keys with; it is the least secure option `sshelf` offers.

## Where secrets live

- **Primary — OS keyring:** macOS Keychain (`security-framework`) or Linux Secret Service
  over D-Bus (`keyring` crate). Service `sshelf`, account = host `id`.
- **`age` vault (opt-in / headless):** if `SSHELF_VAULT_PASSPHRASE` is set, secrets go in the
  XDG data dir as `vault.age`, encrypted with that passphrase (`age` passphrase mode = **scrypt**
  KDF + ChaCha20-Poly1305). This is the path for headless Linux with no Secret Service daemon,
  and for automation/CI. v1 reads the passphrase from the env var (deterministic, scriptable);
  an interactive prompt + auto-detection of a missing keyring are future enhancements.
- **Provisioning:** `sshelf set-password <name|id>` stores a secret from stdin (so it can be
  piped in headless setups) without going through the TUI.
- **Never** in `hosts.toml`, `state.json`, logs, shell history, or process arguments.

The host-key id is the lookup key in both stores, so renaming a host keeps its secret.

## How the password reaches `ssh`

Via `SSH_ASKPASS` — `ssh` calls our helper, which prints the secret on stdout. The password
is **never** passed as a CLI argument (no `sshpass -p`), so it never appears in `ps`/argv. See
[`ssh-command.md`](./ssh-command.md). The helper matches the *shape* of OpenSSH's standard
prompts — a login password (`…password:`) or a key passphrase (`Enter passphrase for key …`) —
and declines host-key confirmations, OTP/verification codes, and arbitrary server text, so a
keyboard-interactive server can't phish the stored secret by merely mentioning "password".

## Threat model

### Protected against
- **On-disk plaintext exposure** — secrets are in the OS keyring or encrypted at rest in the vault.
- **Process-listing / argv leakage** — password is delivered via stdin/stdout to `ssh`, not argv.
- **Shell-history leakage** — `sshelf` never echoes the command containing a password.
- **Casual file snooping** — the vault requires the master passphrase (memory-hard KDF).
- **`hosts.toml` sharing** — it contains no secrets, so it's safe to commit/share/back up.
- **Config-file corruption** — atomic writes; a crash mid-write leaves the prior file intact.

### NOT protected against (out of scope)
- **A root/admin attacker or malware on the machine** — can read process memory, the keyring,
  or keystrokes. `sshelf` assumes you trust your own machine.
- **Keyloggers** — can capture the master passphrase as you type it.
- **A compromised OS keyring daemon** — we trust the platform's secret service.
- **Physical theft without full-disk encryption** — use FDE; that's an OS-level control.
- **Unencrypted backups / cloud sync of `vault.age`** — the vault is encrypted, but treat it
  as sensitive; don't rely on it as your only protection in an untrusted backup.

Assumption: `sshelf` targets a developer/operator's own (trusted) machine, not shared or
hostile hosts.

## Operational notes

- **No password recovery.** Forgetting the vault master passphrase means losing vault secrets.
  Use a passphrase you can recover (e.g. from another password manager).
- **macOS unsigned builds:** the re-exec'd askpass child reading Keychain can trigger an OS
  approval prompt on each connect (Keychain ACLs are keyed to code signature). Ad-hoc sign dev
  builds; release builds should be signed.
- **`StrictHostKeyChecking=accept-new`** trusts a *new* host's key on first connect but still
  hard-fails if a *known* host's key changes (MITM protection retained).
- **Network:** `sshelf` makes no network connections of its own and has no telemetry; it only
  ever launches the OpenSSH tools — `ssh` to connect, and `ssh`/`sftp`/`scp` for the file-transfer
  screen. Transfers authenticate exactly as connect does (keys/agent, or the stored secret via
  `SSH_ASKPASS`) by opening **one** multiplexed `ssh` ControlMaster and running `sftp`/`scp` over
  it — so no extra secret handling, and the secret still never reaches argv. Remote paths are
  shell-quoted before they reach `sftp`/`scp`, control characters are stripped from displayed
  names, and `StrictHostKeyChecking=accept-new` applies there too.

## Reporting

(M8) Add a `SECURITY.md` at the repo root with a disclosure contact before public release.
