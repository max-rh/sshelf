# sshelf

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

Most SSH managers read or rewrite `~/.ssh/config`. `sshelf` deliberately doesn't: it maintains
an independent database, so it never risks corrupting a config shared with Ansible/Terraform/
your editor, and adds things plain SSH config can't express as nicely:

- **Atuin-style fuzzy launcher** — type to filter, `Enter` to connect.
- **Dual-pane file transfer** (`Ctrl-t`) — a two-pane browser to copy files and folders to and
  from a host over SFTP/SCP, with fuzzy search on both sides and live progress. It authenticates
  once (reusing the host's keys/agent or stored password) and never touches `~/.ssh/config`.
- **Background port forwarding** (`Ctrl-f`) — start a Local (`-L`), Remote (`-R`) or Dynamic
  (`-D` SOCKS) SSH tunnel that **keeps running after you quit sshelf**. A manager screen (`F4`)
  lists every active forward and stops any; they're reconciled against the OS on each launch.
- **Guided add/edit form** — hostname, user, port, auth, jump hosts, tags, site, 2FA, extra args.
- **Sites** (`F3`) — group hosts (one per host, e.g. a data center) and optionally give the site
  a **shared bastion + default user/port/key** that members inherit at connect time. The list
  groups by site; `site:NAME` filters. Distinct from the many-valued, free-form tags.
- **Auto-supplied passwords** for password-auth hosts (via `SSH_ASKPASS`; no `sshpass`, the
  secret never appears in `ps`). Stored in your OS keyring, or an encrypted vault.
- **2FA hosts** — flag a host that wants an interactive verification code (TOTP /
  keyboard-interactive) and sshelf prompts for it on connect, feeding it through the same askpass
  channel (manual entry; no stored TOTP seeds).
- **Jump hosts** (`ProxyJump`), **tags/groups** (`tag:prod`), and **frecency** ordering
  (most-used-recently float to the top).
- **Read-only import** from `~/.ssh/config` to get started.

## Install

macOS and Linux, on x86_64 and arm64. The prebuilt installs below need **no Rust toolchain**;
at runtime sshelf wants **OpenSSH 8.4+** (for password auto-supply).

**Homebrew** (macOS or Linux):

```sh
brew install max-rh/tap/sshelf
```

**Shell installer** (prebuilt binary, picks your platform):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/max-rh/sshelf/releases/latest/download/sshelf-installer.sh | sh
```

**Debian/Ubuntu** — grab the `.deb` for your architecture from the
[latest release](https://github.com/max-rh/sshelf/releases/latest), then:

```sh
sudo apt install ./sshelf_*_amd64.deb      # or *_arm64.deb
```
**Gentoo** — via the community-maintained Masterwolf overlay
(unofficial; thanks to @masterwolf-git):

add Masterwolf repository `eselect repository enable masterwolf`, then:

```sh
emerge --sync   # if needed
emerge --ask app-admin/sshelf
```

**Fedora / RHEL / Rocky / openSUSE** — grab the `.rpm` for your architecture from the
[latest release](https://github.com/max-rh/sshelf/releases/latest), then:

```sh
sudo dnf install ./sshelf-*.x86_64.rpm     # or .aarch64.rpm
```

**Cargo** (from [crates.io](https://crates.io/crates/sshelf); needs **Rust 1.88+**):

```sh
cargo install sshelf
```

> Linux uses a pure-Rust Secret Service backend (no `libdbus`/OpenSSL build deps).

> Shell tab-completion (subcommands + flags) ships with every package. After installing, **open a
> new shell** (or `exec $SHELL`) so it loads. For completion of your saved **host names**, see
> [Shell completions](#shell-completions) below.

## Usage

```sh
sshelf                       # launch the TUI
sshelf <host>                # connect straight to a saved host by name (skips the TUI)
sshelf -                     # reconnect to the most recently used host
sshelf add                   # open the TUI add form
sshelf add <name> -H <host>  # add a host non-interactively (see "Adding hosts from the CLI")
sshelf print-command <host>  # print the generated ssh command without connecting
sshelf list                  # print saved hosts (with a ·site· column)
sshelf list <query>          # filter: fuzzy text and/or tag:NAME / site:NAME (e.g. site:prod-dc)
sshelf list --json [query]   # machine-readable output (host fields + the generated command)
sshelf sites                 # list sites (member counts + shared defaults)
sshelf sites add <name> -u deploy -J bastion   # define a site with shared defaults
sshelf --config FILE         # use a specific config file (also: $SSHELF_CONFIG)
sshelf --transfer-log FILE   # log transfer ssh/sftp commands + errors to FILE (debugging)
sshelf import [--dry-run]    # read-only import from ~/.ssh/config
echo "$PASS" | sshelf set-password <name>   # store a password (scriptable / headless)
```

**Keys:** type to filter · `tag:NAME` / `site:NAME` to filter · `↑/↓` move · `Enter` connect ·
`Ctrl-a` add · `Ctrl-e` edit · `Ctrl-d` delete · `Ctrl-y` yank the `ssh` command · `Ctrl-t`
transfer files · `Ctrl-f` port-forward · `Ctrl-o` import · `F1` help · `F2` settings · `F3` sites ·
`F4` forwards · `Esc`/`Ctrl-c` quit.

In the **add/edit** form the Key field picks an identity: `←/→` cycles keys found in `~/.ssh`
(including `.pem`), and `Enter` opens a file browser (type to fuzzy-filter) to choose a key
anywhere. **F2** opens settings (config & hosts-file locations).

On connect, `sshelf` hands the terminal to `ssh` (it `exec`s into it); when the session ends
you're back at your shell.

## Adding hosts from the CLI

`sshelf add` with no arguments opens the TUI form. With any argument it adds a host
non-interactively (handy for scripts and dotfiles); `NAME` and `--hostname` are required:

```sh
# key auth (a -i identity implies --auth key)
sshelf add prod-web -H 10.25.25.10 -u deploy -i ~/.ssh/id_ed25519 -t prod,web

# jump host + custom port
sshelf add prod-db -H 10.25.25.25 -u mike -p 5432 -J bastion.example.net -i ~/.ssh/db

# agent auth (the default) with extra raw ssh flags
sshelf add edge -H edge.example.net --extra "-o ServerAliveInterval=30"

# password auth — pipe the secret in (kept out of argv/history; stored in the keyring or vault)
echo "$PASS" | sshelf add legacy -H 10.0.0.9 -u root --password-stdin
```

| Flag | Meaning |
|---|---|
| `NAME` (positional) | Alias to search/connect by. **Required.** |
| `-H, --hostname <HOST>` | IP or DNS name. **Required.** |
| `-u, --user <USER>` | Login user (defaults to `$USER` at connect time). |
| `-p, --port <PORT>` | SSH port (default 22). |
| `-a, --auth <agent\|key\|password>` | Auth method. Inferred as `key` with `--identity`, `password` with `--password-stdin`, else `agent`. |
| `-i, --identity <PATH>` | Identity file for key auth (repeatable). |
| `-J, --jump <HOST>` | ProxyJump host (repeatable or comma-separated). Key/agent auth only. |
| `-t, --tag <TAG>` | Tag (repeatable or comma-separated). |
| `--extra "<ARGS>"` | Extra raw ssh flags, appended verbatim. |
| `--password-stdin` | Read a password / key passphrase from stdin and store it. |

A duplicate name is refused. To store a secret after the fact instead of `--password-stdin`,
use `sshelf set-password <name>`.

## Shell completions

The packages install static completion (subcommands + flags), also printed by
`sshelf completions <shell>`. For **host-name** completion — `sshelf <Tab>` completing your saved
hosts — enable dynamic completions:

```sh
source <(COMPLETE=bash sshelf)   # bash — add to ~/.bashrc
source <(COMPLETE=zsh sshelf)    # zsh  — add to ~/.zshrc (after compinit)
COMPLETE=fish sshelf | source    # fish — add to ~/.config/fish/config.fish
```

After that, `sshelf prod<Tab>` completes your `prod-*` hosts; the same works after
`sshelf print-command` and `sshelf set-password`.

## Configuration

`~/.config/sshelf/config.toml` (written with comments on first run):

| Key | Default | Meaning |
|---|---|---|
| `decay_rate` | `0.2` | Frecency decay per day (higher = recency matters more). |
| `default_sort` | `"frecency"` | Idle list order: `"frecency"` or `"name"`. |
| `accent` | `"cyan"` | UI accent color. |
| `hosts_file` | (config dir) | Custom host-database path. Editable via **F2** settings; `~` is expanded. |

Point sshelf at an alternate config with `--config FILE` or `$SSHELF_CONFIG` (the config-file
location itself isn't stored in the config — that'd be circular). The hosts-file location *is*
a setting, editable from the **F2** settings screen.

Data lives under XDG dirs: hosts in `~/.config/sshelf/hosts.toml` (human-readable),
usage state in `~/.local/share/sshelf/`.

## Passwords & security

Prefer SSH keys / agent where you can. For password-auth hosts, `sshelf` stores the secret in
your **OS keyring** by default (macOS Keychain, Linux Secret Service). On headless systems with
no keyring, set `SSHELF_VAULT_PASSPHRASE` to use an **age-encrypted vault** instead. Passwords
are never written to `hosts.toml` and never passed on the command line.

`sshelf` makes **no network calls of its own** — no telemetry, no account, no cloud. The only
network activity is the `ssh` it hands your terminal to. See [`SECURITY.md`](SECURITY.md) for
the full threat model.

## Support

If sshelf is useful to you, a Bitcoin tip is appreciated (entirely optional):

[![Donate BTC](https://img.shields.io/badge/Donate-Bitcoin-f7931a?logo=bitcoin&logoColor=white)](bitcoin:bc1qcdeyhpwq76u97dhymx876n49uq85z4y3ccrpje)

**Bitcoin:** `bc1qcdeyhpwq76u97dhymx876n49uq85z4y3ccrpje`

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option —
the Rust-ecosystem norm.

## Documentation

Architecture, data model, and design decisions live in [`docs/`](docs/index.md).
