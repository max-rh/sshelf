# Configuration

## `config.toml`

Lives at `~/.config/sshelf/config.toml` (honors `XDG_CONFIG_HOME`) and is written with
comments on first run:

| Key | Default | Meaning |
|---|---|---|
| `decay_rate` | `0.2` | Frecency decay per day (higher = recency matters more). |
| `default_sort` | `"frecency"` | Idle list order: `"frecency"` or `"name"`. |
| `accent` | `"cyan"` | UI accent color: black / red / green / yellow / blue / magenta / cyan / white / gray. |
| `hosts_file` | (config dir) | Custom host-database path; `~` is expanded. Editable from `F2`. |

Point sshelf at an alternate **config file** with `--config FILE` or `$SSHELF_CONFIG` — the
config-file location itself isn't a key in the file (that would be circular). The **hosts
file** location *is* a setting.

## The settings screen (`F2`)

- **Config file** — shown read-only (it's chosen before the config is read; see above).
- **Hosts file** — editable; blank means the default under the config dir. On save, an
  *existing* file at the new path is **adopted** (loaded, never overwritten); a new path is
  created from your current hosts, so they follow.

## Where everything lives

XDG paths on macOS **and** Linux (`~/.config` / `~/.local/share`, honoring
`XDG_CONFIG_HOME` / `XDG_DATA_HOME`):

| File | Default location | What it is |
|---|---|---|
| `hosts.toml` | `~/.config/sshelf/` | The host database — human-readable, hand-editable, nice to keep in dotfiles. |
| `config.toml` | `~/.config/sshelf/` | The preferences above. |
| `state.json` | `~/.local/share/sshelf/` | Frecency counters. App-managed; churns. |
| `forwards.json` | `~/.local/share/sshelf/` | Ledger of active [port forwards](port-forwarding.md). App-managed. |
| `vault.age` | `~/.local/share/sshelf/` | Encrypted secret store — only in [vault mode](passwords-2fa.md#where-secrets-live). |

Secrets are **never** in `hosts.toml` — keyring or vault only. All writes are atomic
(temp file + rename), so a crash mid-write can't corrupt your files.

Hand-editing `hosts.toml` is supported — that's also how you give one host **multiple**
identity files. The full schema: [Data model & files](data-model.md).
