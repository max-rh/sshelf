# UX: screens, keys, wizard, theming

Visual model is atuin.sh: slim chrome, an inline filter-as-you-type list, and a contextual
keybind hint bar at the bottom.

## Main screen (host list)

```
┌ sshelf  3/14 ───────────────────────────────────────┐
│ > prod                                               │   ← live fuzzy filter input (top)
└──────────────────────────────────────────────────────┘
┌──────────────────────────────────────────────────────┐
│ ▸ prod-web       deploy@web1:2222       [prod]       │   ← selected row, matched chars bold
│   prod-db        mike@10.25.25.25       [prod,db]    │
│   prod-cache     mike@10.0.0.9          [prod]       │
└──────────────────────────────────────────────────────┘
 ↵ connect  ^a add  ^e edit  ^d delete  ^y yank  ^o import  F1 help  esc quit
```

Layout: `Length(3)` search · `Min(0)` list · `Length(1)` hint bar. Each row shows
`name · user@host[:port] · [tags]`. The `matched/total` count lives in the search-box
title (so it's never truncated by a narrow terminal).

### Sorting / ranking

- **No query (idle):** sort by **frecency** desc (most-used-recently first).
  `score = use_count * exp(-decay_rate * days_since_last_used)`, `decay_rate` default `0.2`.
- **Typing a query:** fuzzy-filter via `nucleo-matcher`; sort by match score; **frecency
  breaks ties**. Matched characters are highlighted (bold/accent) using the matcher's match
  indices, rendered with `unicode-width` so wide/combining chars don't misalign.
- v1 ships fuzzy search only (prefix/substring modes can come later).

## Keybindings (list screen)

The search box is **always active** (atuin-style single mode), so plain letters filter the
list. Actions therefore use **Ctrl** (or function keys) to avoid being typed into the query.

| Key | Action |
|---|---|
| _type_ | filter the list (fuzzy) |
| `tag:NAME` | filter by tag; combine with text and repeat (`tag:prod tag:db`, AND) |
| `↑` / `↓`, `Ctrl-p` / `Ctrl-n` | move selection |
| `Enter` | connect to selected host (tears down TUI, `exec()`s ssh) — M3 |
| `Ctrl-a` | add host (wizard) — M4 |
| `Ctrl-e` | edit selected host — M4 |
| `Ctrl-d` | delete selected host (confirm) — M4 |
| `Ctrl-y` | yank — copy/print the generated `ssh` command without connecting — M3 |
| `Ctrl-t` | open the dual-pane **file-transfer** screen for the selected host |
| `Ctrl-o` | import from `~/.ssh/config` (read-only) — M7 |
| `F1` | help overlay |
| `F2` | settings (config & hosts-file locations) |
| `Esc` | clear the query if non-empty, otherwise quit |
| `Ctrl-c` | quit |

Implemented in M2: type-to-filter, navigation, `Enter` (returns a Connect outcome), `F1`
help, `Esc`/`Ctrl-c`. The action keys show a "coming in Mx" status until their milestone.
Tag filtering and `config.toml` keybinding overrides land in M6.

## Add / Edit form

A single full-screen form (`Ctrl-a` add, `Ctrl-e` edit selected). Every field shows a dim
**placeholder** explaining it. The form is **auth-aware** — it shows only the fields relevant
to the chosen Auth method, so the rest don't clutter the screen.

Always shown: **Name** (required), **Hostname** (required), **User** (defaults `$USER`),
**Port** (defaults 22), **Auth**, **Jump hosts**, **Tags**, **Extra args** (raw flags escape hatch).

Auth-specific fields:

| Auth | Extra fields |
|---|---|
| `agent` | none |
| `key` | **Key** — `←`/`→` cycles private keys found in `~/.ssh`, **`Enter` opens a file browser** to pick a key anywhere (e.g. a `.pem` in `~/Downloads`); **Key passphrase** — optional, only if the key is encrypted |
| `password` | **Password** |

Key discovery finds keypairs (`<name>.pub` sibling) **and** standalone private keys including
`.pem` (detected by a `PRIVATE KEY` header), so AWS-style keys show up too. Every field shows a
dim placeholder, prefixed `optional ·` when the field can be left blank (`required ·` for Name/Hostname).

**File browser** (opened from the Key field with `Enter`): a modal listing the current
directory with a fuzzy filter — **type to filter**, `↑`/`↓` move, `Enter` opens a directory or
selects a file, `←` goes up, `Backspace` edits the filter (or goes up when empty), `Esc` clears
the filter (or cancels when empty). It starts in `~/.ssh` (or near the current key) and a picked
path is stored as the host's identity, even if it's outside `~/.ssh`. Key discovery finds `.pem`
and other private keys by their header, not just `.pub` pairs.

## Settings (`F2`)

A screen for configuring sshelf itself. v1:

- **Config file** — shown read-only (it's chosen *before* the config is read, via `--config` /
  `$SSHELF_CONFIG`, so it can't be a setting in the file itself).
- **Hosts file** — editable; blank means the default under the config dir. `~` is expanded.
  On save, an *existing* file at the new path is **adopted** (loaded, never overwritten); a new
  path is created from the current hosts so they follow. More settings will be added here.

Navigation: `Tab` / `↑` / `↓` move between fields; `←` / `→` (or space) change the choosers
(Auth, Key); `Enter` advances and **saves on the last field**; `Ctrl-s` saves anywhere; `Esc`
cancels. Validation errors (missing name/hostname, non-numeric port) show inline and focus
jumps to the offending field.

> Implemented as a single-screen, auth-aware field form (not a paged wizard) — simpler to
> navigate/edit and "guided" by placeholders + inline validation. The Key field is a picker
> (single key); a host configured with **multiple** identity files keeps them on edit, but
> entering several keys is done by editing `hosts.toml`.

**Quick-add:** the form opens with defaults, so typing a Name + Hostname and `Ctrl-s` is enough.

**Secrets (Password / Key passphrase):** the masked value is stored in the OS keyring (or the
age vault) keyed by host id — **never** in `hosts.toml`. On edit, leaving it blank keeps the
existing secret. It's auto-supplied at connect time (see `ssh-command.md`). Deleting a host
(`Ctrl-d`) removes the host, its frecency entry, and its stored secret.

## Import (`Ctrl-o` / `sshelf import`)

Parses `~/.ssh/config` **read-only** via `ssh2-config` and adds every host whose name isn't
already present, warning about unsupported `Match` / `Include` / `ProxyJump`. v1 imports all
new hosts at once (no per-host selection screen) — curate afterwards with `Ctrl-e` / `Ctrl-d`.
The CLI form supports `--dry-run` to preview. Never writes back to `~/.ssh/config`.

## File transfer (`Ctrl-t`)

`Ctrl-t` on a host opens a **dual-pane transfer screen**: local files on the left, the host's
files on the right. sshelf authenticates **once** by opening an `ssh` ControlMaster that reuses
the host's auth (keys/agent/ProxyJump, or the stored keyring/vault secret via `SSH_ASKPASS`),
then runs `sftp` (`ls`/`get`/`put`) over it. Remote listing and transfers run on a background
thread, so the UI stays responsive on slow links.

Both panes fuzzy-filter as you type:

| Key | Action |
|---|---|
| _type_ | filter the focused pane |
| `Tab` | switch the focused pane (local ↔ remote) |
| `↑` / `↓`, `Ctrl-p` / `Ctrl-n` | move the selection |
| `→` / `Enter` | open the selected directory (or send a file) |
| `Ctrl-s` | **send** the selected file or folder (recursive) into the *other* pane's directory |
| `←` | go up a directory |
| `Backspace` | edit the filter, or go up when it's empty |
| `Esc` | cancel a running transfer, else clear the filter, else close the screen |

A progress bar shows bytes and percent for single-file downloads; folder and upload transfers
show as in-flight (cancelable with `Esc`). Directories are marked `name/` and symlinks `name@`;
symlinks are skipped in this version. Filenames are shell-quoted and control characters stripped
from display. The connection uses `StrictHostKeyChecking=accept-new`, like connect — so a
first-time host key is trusted on use (see [`security.md`](security.md)). Only one transfer runs
at a time in v1, and a same-named file or folder already present in the destination is **skipped**
(with a message) rather than overwritten.

On failure the status line shows the underlying `sftp` error. For more detail, run with
`sshelf --transfer-log <FILE>` (or `$SSHELF_TRANSFER_LOG=<FILE>`): the worker appends every
`ssh`/`sftp` command and its full stderr to that file. The log holds no secrets — the password
reaches `ssh` via `SSH_ASKPASS`, never the command line.

## CLI (outside the TUI)

| Command | What it does |
|---|---|
| `sshelf` | Launch the interactive TUI. |
| `sshelf <host>` | Connect straight to a saved host by **name or id**, skipping the TUI — same connect path as `Enter` (frecency recorded before the `exec`, secret auto-supplied). A miss suggests close names; a name that collides with a subcommand (`list`, `import`, …) is reached via the TUI instead. |
| `sshelf -` | Reconnect to the most-recently-used host (max `last_used` in the frecency state). Errors (without connecting) if there's no history yet. |
| `sshelf add` | With **no args**, opens the TUI add form. With **args**, adds a host non-interactively: `NAME` + `-H/--hostname` required; `-u/--user`, `-p/--port`, `-a/--auth`, `-i/--identity` (repeatable, implies key auth), `-J/--jump`, `-t/--tag`, `--extra "<flags>"`, `--password-stdin` (reads the secret from stdin). Duplicate names are refused. |
| `sshelf print-command <host>` | Print the generated, shell-quoted `ssh …` command for a saved host by **name or id**, without connecting or changing frecency. CLI equivalent of the TUI's `Ctrl-y` yank. |
| `sshelf list [query]` | List hosts. `query` filters with the TUI's syntax — fuzzy text and/or `tag:NAME` (e.g. `sshelf list tag:prod`, `sshelf list web`). |
| `sshelf list --json [query]` | Same selection, emitted as JSON (each host's fields + its generated `command`). Always valid JSON, even when empty — the stable surface for scripts/integrations. |
| `sshelf import [--dry-run]` | Read-only import from `~/.ssh/config`. |
| `sshelf set-password <host>` | Store a password (read from stdin) for a host. |
| `sshelf completions <shell>` · `sshelf man` | Emit static completions / the man page. |
| `--config FILE` (global) | Use a specific config file (also `$SSHELF_CONFIG`). |
| `--transfer-log FILE` (global) | Append transfer-screen diagnostics — the `ssh`/`sftp` commands and their errors (no secrets) — to `FILE`. Also `$SSHELF_TRANSFER_LOG`. |

**Dynamic completion (host names):** static completions cover subcommands/flags only. Sourcing
`COMPLETE=<shell> sshelf` (e.g. `source <(COMPLETE=zsh sshelf)`) enables host-name completion via
clap_complete's engine — `host_name_candidates` reads `hosts.toml` (side-effect-free) and is
attached to the `<host>` arguments of direct-connect, `print-command`, and `set-password`.

## Confirmations & overlays

- **Delete** pops a confirm modal (`y` = delete, any other key = cancel).
- **Help** (`F1`) is an overlay listing all keys; any key closes it.

## Theming

atuin-inspired defaults: dim chrome, a single accent color for selection + match highlights.
Terminal resize is handled automatically by ratatui's layout — no manual recompute.

## Configuration (`config.toml`)

A commented default is written on first run. Keys:

| Key | Default | Meaning |
|---|---|---|
| `decay_rate` | `0.2` | Frecency decay per day (higher = recency matters more). |
| `default_sort` | `"frecency"` | Idle list order: `"frecency"` or `"name"`. |
| `accent` | `"cyan"` | Accent color: black/red/green/yellow/blue/magenta/cyan/white/gray. |

Location: `~/.config/sshelf/config.toml` (honors `XDG_CONFIG_HOME`).
