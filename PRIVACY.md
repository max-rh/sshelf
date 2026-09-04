# Privacy

sshelf runs on your machine and nowhere else. There is no account, no server of mine, and no
telemetry. This page says exactly what it reads, writes, runs, and sends, so you don't have to
take that on faith. The attacker-facing view (what stored secrets are and aren't protected
against) is in [SECURITY.md](SECURITY.md).

## What it reads

- Its own files: `hosts.toml` and `config.toml` under `~/.config/sshelf/`, and
  `state.json` / `forwards.json` under `~/.local/share/sshelf/`.
- `~/.ssh/config`, but only when you ask. `sshelf import` (or `Ctrl-o`) parses it and copies
  what it can model into sshelf's own database. It is opened read-only and never written back.
- The file names in `~/.ssh`, so the add form can offer your keys in a picker. For a file
  with no `.pub` sibling it reads the first 64 bytes to see whether the header says
  `PRIVATE KEY`. It keeps the path. It never reads a key's contents beyond that header, and
  never copies a key anywhere.

## What it writes

- Its own files, the four above, always by atomic replace so a crash can't shred them.
- `~/.config/sshelf/ssh_config`, the export fragment, but only after you run
  `sshelf export` once. From then on it is rewritten whenever your hosts change.
- One keyring entry per host you gave a password to, under the service name `sshelf`,
  keyed by the host's id. In vault mode it's a line in `vault.age` instead.
- Nothing under `~/.ssh`. Not `config`, not anything else, not ever. (The `ssh` sshelf
  launches still maintains your `known_hosts` itself, exactly as it always has.)

## What it runs

`ssh` to connect and to hold port forwards, `ssh` plus `sftp` for the transfer screen, `tmux`
if you turned on tmux mode, and `tailscale status --json` if you run
`sshelf import --tailscale`. That's the whole list. Each one runs because you pressed a key or
typed a command, never at startup, never on a timer, never in the background.

## What it sends

Nothing. No telemetry, no analytics, no crash reports, no update check, no account, no license
check, no "anonymous usage statistics". sshelf opens no network connection of its own, so
there is nothing to opt out of. The only traffic is the SSH session you asked for, from your
machine straight to your server.

That is also why there are no live health dots, no fleet metrics, and no cloud-provider
sync: each of those would mean sshelf talking to something on its own.

## Where secrets live

In your OS keyring (macOS Keychain, or the Secret Service on Linux), or in an
`age`-encrypted `vault.age` if you set `SSHELF_VAULT_PASSPHRASE` for a headless box. Keyed by
host id, so renaming a host keeps its password.

Never in `hosts.toml`, which is safe to commit and share. Never on a command line, so
never in `ps` or your shell history. Never in a log: passwords reach `ssh` through
`SSH_ASKPASS`, and even the optional transfer log records only commands and their errors.

## How to check

- Run `sshelf doctor`. It reports which backend holds your secrets and where your files
  are, and it does it without contacting a host.
- Read `hosts.toml`. It's TOML, it's yours, and everything sshelf knows about a host is in it.
- Watch it: `lsof`, Little Snitch, or your firewall of choice will show sshelf's own process
  making no connections.
- The reasoning behind all of this is in the
  [decision log](https://max-rh.github.io/sshelf/decisions.html).

Something here wrong or unclear? Open an issue. I'd rather fix the page than have you guess.
