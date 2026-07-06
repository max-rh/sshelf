# FAQ & troubleshooting

## Why doesn't sshelf just use my SSH config?

By design. `~/.ssh/config` is often shared, load-bearing infrastructure — Ansible, Terraform,
your editor's remote mode all read it — and a tool that rewrites it can corrupt all of that.
sshelf keeps an **independent database** and builds `ssh` commands from it with plain flags;
its only contact with your SSH config is the explicit, [read-only import](import.md). (ssh
itself still *reads* your config normally when sshelf launches it — sshelf just never writes
it.)

## What does sshelf need at runtime?

**OpenSSH 8.4+** on your machine — password/passphrase auto-supply rides on
`SSH_ASKPASS_REQUIRE`, added in OpenSSH 8.4 (2020). Check with `ssh -V`. Key/agent hosts work
with anything reasonably modern. Platforms: macOS + Linux, x86_64 and arm64.

## Does sshelf phone home?

No. No telemetry, no account, no network calls of its own — the only network activity is the
`ssh`/`sftp` it runs for you. See [Security](security.md).

## Password auto-supply isn't working

- Check `ssh -V` — you need OpenSSH 8.4+ (see above).
- **Built from source on macOS?** An unsigned binary can hit a Keychain approval prompt on
  every connect (Keychain ACLs are keyed to the code signature). Approve it, or ad-hoc sign
  your build: `codesign -s - target/release/sshelf`.

## I'm on a headless box with no keyring

Set `SSHELF_VAULT_PASSPHRASE` — secrets then live in an `age`-encrypted vault file instead of
a keyring. Details: [where secrets live](passwords-2fa.md#where-secrets-live); the
env-inheritance tradeoff is documented in [Security](security.md).

## Can a jump host use password auth?

Not currently — jump hosts are key/agent only. The askpass helper holds the *target's* secret
and can't tell which hop in a chain is prompting.

## My 2FA host fails before I can type the code

Flag it: **2FA = yes** in the edit form (or `--2fa` on `sshelf add`). A stored-secret connect
routes *all* prompts to the askpass helper with no terminal fallback, so the verification
prompt needs the [2FA flow](passwords-2fa.md#two-factor-2fa-hosts) to answer it.

## Tab completion doesn't complete my host names

Completion has two layers. The packages install **static** completion (subcommands + flags) —
open a new shell so it loads. Completing your saved **host names** needs the dynamic engine
sourced in your shell rc — one line per shell: [Shell completions](cli.md#shell-completions).

## A forward vanished from F4

`F4` only ever shows forwards whose processes are **actually running** — the list is
reconciled against the OS on launch and refreshed live while open. If the tunnel died (
reboot, sleep, network drop, killed from another terminal), it leaves the list; start it
again with `Ctrl-f`. Automatic re-launch of dropped forwards isn't there yet.

## How do I back up or sync my hosts?

`hosts.toml` is one human-readable TOML file — keep it in your dotfiles like any config (a
custom path is a setting: [Configuration](configuration.md)). **Secrets don't travel with
it**: they're per-machine, in each machine's keyring or vault — re-add them with
`sshelf set-password`. Frecency state is per-machine and app-managed.

## Where did the first-connection host-key prompt go?

Connections pass `StrictHostKeyChecking=accept-new`: a brand-new host's key is accepted and
recorded on first use (so the prompt can't interfere with automated password supply), while a
**changed** key for a known host still hard-fails, as ever. The tradeoff is discussed in
[Security](security.md).

## Windows?

Not currently — connect hands off via Unix `exec()`, and the askpass/process plumbing is
Unix-specific. macOS + Linux for now.
