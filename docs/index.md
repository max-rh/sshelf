# sshelf

A fast terminal UI for managing and connecting to SSH hosts. Save each node once, then
fuzzy-search and connect in two keystrokes.

![sshelf: fuzzy-search your SSH hosts and connect in two keystrokes](./sshelf-readme.gif)

**sshelf keeps its own host database and generates the correct `ssh` command for you. It
never reads or edits `~/.ssh/config`** (except an explicit, read-only import). No account, no
cloud, no telemetry: your hosts live in a human-readable TOML file on your disk, secrets live
in your OS keyring, and the only network activity is the `ssh` it hands your terminal to.

## Get started

```sh
brew install max-rh/tap/sshelf     # macOS or Linux
sshelf                             # launch the TUI
```

- [Install](install.md): Homebrew, shell installer, `.deb`, `.rpm`, Gentoo, or cargo.
- [Quickstart](quickstart.md): the first five minutes, adding or importing hosts, connecting.
- [FAQ & troubleshooting](faq.md): common questions, quick answers.
- [`sshelf doctor`](doctor.md): when something isn't working, run this first.

## What's in the box

- An atuin-style fuzzy launcher with frecency ordering.
  [Searching & connecting](search-connect.md)
- A dual-pane SFTP file browser (`Ctrl-t`): mark several, send in one go, `F7` to create a
  directory. [Transferring files](transfer.md)
- tmux mode, where `Enter` opens each host in a new window or pane and keeps the picker up.
  [Connecting inside tmux](search-connect.md#connecting-inside-tmux)
- Background port forwards that survive quitting (`Ctrl-f` / `F4`).
  [Port forwarding](port-forwarding.md)
- Sites with a shared bastion and defaults, plus free-form tags (`F3`).
  [Sites & tags](sites-tags.md)
- Stored passwords and passphrases auto-supplied at connect, and 2FA code prompts.
  [Passwords, keys & 2FA](passwords-2fa.md)
- SSH-config export: one `Include` line and plain `ssh`/`scp`, rsync, and VS Code Remote see
  your hosts. [Exporting to SSH config](export.md)
- Import from `~/.ssh/config` or your whole Tailscale tailnet (`--tailscale`).
  [Importing hosts](import.md)
- A scriptable CLI (`sshelf add`, `list --json`, `print-command`, ...).
  [CLI reference](cli.md)
- `sshelf doctor`, one command that checks your setup and names the fix.
  [Checking your setup](doctor.md)

Platforms: **macOS + Linux**, x86_64 and arm64. Runtime: **OpenSSH 8.4+** for password
auto-supply.

## How it's built

Deciding whether to trust it, or just curious how the pieces fit?

- [Security & threat model](security.md): exactly what stored secrets are protected
  against, and what they are not.
- [How the ssh command is built](ssh-command.md): argv generation and the `SSH_ASKPASS`
  mechanism that supplies passwords without `sshpass`.
- [Privacy](https://github.com/max-rh/sshelf/blob/master/PRIVACY.md): what sshelf reads,
  writes, runs, and sends, in plain terms. Nothing leaves your machine.

## Contributing

Questions, ideas, and feature requests belong in
[GitHub Discussions](https://github.com/max-rh/sshelf/discussions).

Start with [`CONTRIBUTING.md`](https://github.com/max-rh/sshelf/blob/master/CONTRIBUTING.md),
then the **Development** section in the sidebar: architecture, module map, data model, and the
decision log. Docs follow the docs-in-sync rule: every behavior change updates the relevant
page here in the same change, with a dated entry in the [progress log](progress.md).
