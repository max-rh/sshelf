# sshelf

[![crates.io](https://img.shields.io/crates/v/sshelf.svg)](https://crates.io/crates/sshelf)
[![CI](https://github.com/max-rh/sshelf/actions/workflows/ci.yml/badge.svg)](https://github.com/max-rh/sshelf/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

A fast terminal UI for managing and connecting to SSH hosts. Save each node once, then
fuzzy-search and connect in two keystrokes.

![sshelf — fuzzy-search your SSH hosts and connect in two keystrokes](docs/sshelf-readme.gif)

**`sshelf` keeps its own host database and generates the correct `ssh` command for you — it
never reads or edits `~/.ssh/config`** (except an explicit, read-only import). No more hunting
for the right `ssh -i … -J … user@host` invocation.

```
┌ sshelf  3/14 ───────────────────────────────────────┐
│ > prod                                               │
└──────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────┐
│ ▸ prod-web    deploy@10.25.25.10:22      [prod,web]  │
│   prod-db     mike@10.25.25.25:5432      [prod,db]   │
│   prod-cache  mike@10.0.0.9:22           [prod]      │
└──────────────────────────────────────────────────────┘
 ↵ connect  ^a add  ^e edit  ^d del  ^y yank  ^o import  tag:NAME  F1 help  esc quit
```

## Why sshelf

Most SSH managers read or rewrite `~/.ssh/config`. `sshelf` deliberately doesn't: it keeps an
independent database, so it never risks corrupting a config shared with Ansible/Terraform/your
editor — and it adds what plain SSH config can't express:

- **Fuzzy launcher** — type to filter, `Enter` to connect; your most-used hosts float to the top.
- **Dual-pane file transfer** (`Ctrl-t`) — copy files and folders both ways over SFTP, with
  fuzzy search on both sides and one authentication.
- **Background port forwarding** (`Ctrl-f`) — Local / Remote / SOCKS tunnels that keep running
  after you quit; `F4` lists and stops them.
- **Sites & tags** (`F3`) — group hosts; a site can carry a shared bastion + defaults that
  members inherit at connect time.
- **Auto-supplied passwords** — stored in your OS keyring (or an encrypted vault), fed to `ssh`
  via `SSH_ASKPASS`: never in a file, never visible in `ps`.
- **2FA hosts** — flag a host and sshelf prompts for the verification code on connect.
- **Jump hosts, a guided add/edit form, frecency ordering, read-only import** from `~/.ssh/config`.

**Never:** no telemetry, no account, no cloud — and it will never edit your SSH config.

## Install

macOS and Linux, x86_64 and arm64 — no Rust toolchain needed for the prebuilt packages. At
runtime sshelf wants **OpenSSH 8.4+** (for password auto-supply).

**Homebrew** (macOS or Linux):

```sh
brew install max-rh/tap/sshelf
```

**Shell installer** (prebuilt binary, picks your platform):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/max-rh/sshelf/releases/latest/download/sshelf-installer.sh | sh
```

<details>
<summary><b>More options</b> — Debian/Ubuntu · Fedora/RHEL · Gentoo · cargo</summary>

**Debian/Ubuntu** — grab the `.deb` from the
[latest release](https://github.com/max-rh/sshelf/releases/latest), then
`sudo apt install ./sshelf_*_amd64.deb` (or `*_arm64.deb`).

**Fedora / RHEL / Rocky / openSUSE** — grab the `.rpm` (static build, works on any RPM
distro) from the [latest release](https://github.com/max-rh/sshelf/releases/latest), then
`sudo dnf install ./sshelf-*.x86_64.rpm` (or `.aarch64.rpm`).

**Gentoo** — community-maintained overlay (unofficial; thanks to @masterwolf-git):
`eselect repository enable masterwolf && emerge --sync && emerge --ask app-admin/sshelf`.

**Cargo** (from [crates.io](https://crates.io/crates/sshelf); needs Rust 1.88+):
`cargo install sshelf`.

Shell tab-completion ships with every package — open a new shell after installing. On Linux,
secrets use a pure-Rust Secret Service backend (no `libdbus`/OpenSSL build deps).

</details>

Full details + completions setup: **[Install guide](https://max-rh.github.io/sshelf/install.html)**.

## First five minutes

```sh
sshelf                        # launch the TUI — Ctrl-a adds your first host
sshelf import --dry-run       # preview a read-only import from ~/.ssh/config
sshelf import                 # …do it
sshelf prod-web               # connect straight to a saved host (skips the TUI)
sshelf -                      # reconnect to the most recently used host
sshelf list tag:prod --json   # scriptable listing (fields + generated command)
sshelf print-command db       # print the ssh command instead of running it
```

In the TUI: type to filter (plus `tag:NAME` / `site:NAME`), `Enter` to connect — **`F1` shows
every key**. On connect sshelf hands the terminal to `ssh` (it `exec`s into it); when the
session ends you're back at your shell.

## Documentation

The **[user guide](https://max-rh.github.io/sshelf/)** covers everything:
[Quickstart](https://max-rh.github.io/sshelf/quickstart.html) ·
[CLI reference](https://max-rh.github.io/sshelf/cli.html) ·
[Configuration](https://max-rh.github.io/sshelf/configuration.html) ·
[FAQ](https://max-rh.github.io/sshelf/faq.html) — plus per-feature pages for
[file transfer](https://max-rh.github.io/sshelf/transfer.html),
[port forwarding](https://max-rh.github.io/sshelf/port-forwarding.html),
[sites & tags](https://max-rh.github.io/sshelf/sites-tags.html), and
[passwords & 2FA](https://max-rh.github.io/sshelf/passwords-2fa.html).
Architecture and design decisions live in [`docs/`](docs/index.md).

## Passwords & security

Prefer SSH keys / agent where you can. Stored secrets live in the macOS Keychain / Linux
Secret Service (or an `age`-encrypted vault on headless systems) — never in `hosts.toml`,
never on a command line. `sshelf` makes **no network calls of its own** — no telemetry, no
account, no cloud; the only network activity is the `ssh` it hands your terminal to. See
[`SECURITY.md`](SECURITY.md) for the full threat model.

## Support

If sshelf is useful to you, a Bitcoin tip is appreciated (entirely optional):

[![Donate BTC](https://img.shields.io/badge/Donate-Bitcoin-f7931a?logo=bitcoin&logoColor=white)](bitcoin:bc1qcdeyhpwq76u97dhymx876n49uq85z4y3ccrpje)

**Bitcoin:** `bc1qcdeyhpwq76u97dhymx876n49uq85z4y3ccrpje`

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option —
the Rust-ecosystem norm.
