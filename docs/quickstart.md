# Quickstart

The first five minutes with sshelf.

## 1. Launch

```sh
sshelf
```

The first run writes a commented `config.toml` under `~/.config/sshelf/`. You land in the
(empty) host list — **`F1` shows every key** at any time.

## 2. Add a host — or import the ones you already have

Press **`Ctrl-a`**: the add form opens with sensible defaults, so typing a **Name** and a
**Hostname** and pressing `Ctrl-s` is enough for a first host. Auth, jump hosts, tags, and
the rest are covered in [Adding & editing hosts](hosts.md).

Already have hosts in `~/.ssh/config`? Import them — **read-only**, sshelf copies them into
its own database and never writes your config:

```sh
sshelf import --dry-run    # preview what would be imported
sshelf import              # do it (or press Ctrl-o in the TUI)
```

## 3. Connect

Type a few characters to fuzzy-filter, `Enter` to connect. sshelf records your usage and then
**`exec`s into `ssh`** — the TUI is gone and it's a plain ssh session; when it ends you're
back at your shell. The hosts you use most float to the top of the idle list.

## 4. Connect even faster

```sh
sshelf prod-web            # straight to a saved host by name — no TUI
sshelf -                   # reconnect to the most recently used host
```

## 5. Know where your data lives

Your hosts are one human-readable TOML file — `~/.config/sshelf/hosts.toml` — safe to
hand-edit and to keep in your dotfiles. Secrets are **not** in it: they live in your OS
keyring or an encrypted vault ([Passwords, keys & 2FA](passwords-2fa.md)).

## Where to next

- [Searching & connecting](search-connect.md) — `tag:`/`site:` filters, frecency, yanking
  the generated command.
- [Transferring files](transfer.md) — the dual-pane SFTP browser (`Ctrl-t`).
- [Port forwarding](port-forwarding.md) — background tunnels that outlive the TUI (`Ctrl-f`).
- [CLI reference](cli.md) — scripting: `add`, `list --json`, `print-command`, completions.
