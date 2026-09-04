# FAQ & troubleshooting

## Why doesn't sshelf just use my SSH config?

By design. `~/.ssh/config` is often shared, load-bearing infrastructure (Ansible, Terraform,
and your editor's remote mode all read it), and a tool that rewrites it can corrupt all of that.
sshelf keeps an **independent database** and builds `ssh` commands from it with plain flags;
its only contact with your SSH config is the explicit, [read-only import](import.md). (ssh
itself still *reads* your config normally when sshelf launches it; sshelf just never writes
it.) It isn't a one-way street either: [`sshelf export`](export.md) generates an `Include`
file so plain `ssh`/`scp`, and anything that reads SSH config like VS Code Remote, can use
your sshelf hosts by name.

## What does sshelf need at runtime?

**OpenSSH 8.4+** on your machine. Password and passphrase auto-supply ride on
`SSH_ASKPASS_REQUIRE`, added in OpenSSH 8.4 (2020). Key/agent hosts work with anything
reasonably modern. Platforms: macOS + Linux, x86_64 and arm64.

Run [`sshelf doctor`](doctor.md). It reads your version and says whether it's enough.

## Does sshelf phone home?

No. No telemetry, no account, no network calls of its own. The only network activity is the
`ssh`/`sftp` it runs for you. See [Security](security.md).

## Then how does the Tailscale import work?

The same way: by running a program you already have.
[`sshelf import --tailscale`](import.md#from-tailscale) executes **your** `tailscale` CLI
(`tailscale status --json`) and parses its output. sshelf opens no sockets, holds no API key,
and talks to no Tailscale server. It only ever runs when you type that command: never at
startup, on a save, on a timer, or from the TUI. Nothing tailscale-specific (node IDs, keys) is
written to `hosts.toml`, and the import only ever *adds* hosts.

## Password auto-supply isn't working

- Built from source on macOS? An unsigned binary can hit a Keychain approval prompt on
  every connect (Keychain ACLs are keyed to the code signature). Approve it, or ad-hoc sign
  your build: `codesign -s - target/release/sshelf`.

Start with [`sshelf doctor`](doctor.md): it checks the OpenSSH version *and* round-trips your
secret backend, which covers both of the usual causes.

## I'm on a headless box with no keyring

Set `SSHELF_VAULT_PASSPHRASE` and secrets then live in an `age`-encrypted vault file instead
of a keyring. Details: [where secrets live](passwords-2fa.md#where-secrets-live); the
env-inheritance tradeoff is documented in [Security](security.md).

[`sshelf doctor`](doctor.md) confirms which backend it's actually using and whether it opens.

## Can a jump host use password auth?

Not currently; jump hosts are key/agent only. The askpass helper holds the *target's* secret
and can't tell which hop in a chain is prompting. If a jump-host connection fails, check your
agent is reachable: [`sshelf doctor`](doctor.md).

## Can I open connections in tmux windows instead of leaving the picker?

Yes. Set `tmux = "window"` (or `"pane"`) in
[`config.toml`](configuration.md), or cycle the field on the `F2` settings screen. When sshelf
is running inside tmux, `Enter` then opens the host in a new window named after it and keeps
the picker up, so you can fire off several in a row. Outside tmux the setting does nothing.

## Why did my 2FA host not open a tmux window?

Because the verification code would have to travel as `tmux new-window -e KEY=VALUE`, the tmux
client's own command line, which anyone on the machine can read with `ps`. sshelf refuses to
put a one-time code there, so those hosts connect in place instead, printing
`2FA host — connecting here` before the handoff. The same applies to stored-password hosts in
[vault mode](passwords-2fa.md#where-secrets-live) (the master passphrase would cross the same
boundary), and to tmux older than 3.0, which has no `-e` at all. Key, agent, and
keyring-backed password hosts open in tmux normally. Details:
[Connecting inside tmux](search-connect.md#connecting-inside-tmux).

## Can I send more than one file at a time?

Mark them: `Space` toggles a mark in the
[transfer screen](transfer.md#marking-and-sending-several-at-once), `Ctrl-a` marks everything
the filter shows, and `Ctrl-s` sends the lot, folders included and recursively, one at a time
over the one connection. `F7` (or `Ctrl-f`) creates a directory on either side without leaving
the screen.

## My 2FA host fails before I can type the code

Flag it: **2FA = yes** in the edit form (or `--2fa` on `sshelf add`). A stored-secret connect
routes *all* prompts to the askpass helper with no terminal fallback, so the verification
prompt needs the [2FA flow](passwords-2fa.md#two-factor-2fa-hosts) to answer it.

## Tab completion doesn't complete my host names

Completion has two layers. The packages install **static** completion (subcommands + flags),
so open a new shell to load it. Completing your saved **host names** needs the dynamic engine
sourced in your shell rc, one line per shell: [Shell completions](cli.md#shell-completions).

If host names still don't complete, check the database itself parses:
[`sshelf doctor`](doctor.md).

## A forward vanished from F4

`F4` only ever shows forwards whose processes are **actually running**: the list is
reconciled against the OS on launch and refreshed live while open. If the tunnel died (
reboot, sleep, network drop, killed from another terminal), it leaves the list; start it
again with `Ctrl-f`. Automatic re-launch of dropped forwards isn't there yet.

## How do I back up or sync my hosts?

`hosts.toml` is one human-readable TOML file, so keep it in your dotfiles like any config (a
custom path is a setting: [Configuration](configuration.md)). **Secrets don't travel with
it**: they're per-machine, in each machine's keyring or vault, so re-add them with
`sshelf set-password`. Frecency state is per-machine and app-managed.

On the new machine, run [`sshelf doctor`](doctor.md). It checks the file parses, has no
duplicate names or ids, and that its sites all exist.

## Where did the first-connection host-key prompt go?

Connections pass `StrictHostKeyChecking=accept-new`: a brand-new host's key is accepted and
recorded on first use (so the prompt can't interfere with automated password supply), while a
**changed** key for a known host still hard-fails, as ever. The tradeoff is discussed in
[Security](security.md).

## Windows?

Not currently. Connect hands off via Unix `exec()`, and the askpass/process plumbing is
Unix-specific. macOS + Linux for now.

## Something isn't working and I don't know why

Run [`sshelf doctor`](doctor.md). It checks the OpenSSH version, your secret backend, the host
database, dangling site references, the ssh-agent, and the exported ssh_config fragment, and
names a fix for anything that isn't right. It's local and read-only, and it never contacts a host.

## My question isn't here

Ask in [GitHub Discussions](https://github.com/max-rh/sshelf/discussions). Questions, ideas,
and feature requests all belong there, and answers that come up often end up on this page.
