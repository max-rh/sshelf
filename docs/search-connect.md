# Searching & connecting

The list screen is a fuzzy launcher in the style of atuin: the search box is **always
active**, so plain typing filters the list, and actions use **Ctrl** or function keys.

## Filtering

- **Type** — fuzzy match against your hosts; matched characters are highlighted.
- **`tag:NAME`** — only hosts with that tag. Repeat and combine with text
  (`tag:prod tag:db` is an AND).
- **`site:NAME`** — only hosts in that [site](sites-tags.md).

## Ordering

- **Idle (no query):** hosts sort by **frecency** — usage count decayed by recency, so your
  daily drivers sit at the top. The decay rate is configurable, and `default_sort = "name"`
  opts out entirely ([Configuration](configuration.md)). The idle list also **groups by
  site** (`── site (n) ──` headers, `(no site)` last).
- **Filtering:** best fuzzy match first; frecency breaks ties. The list is flat, with a dim
  `·site·` column.

## Keys

| Key | Action |
|---|---|
| _type_ | filter the list (fuzzy text, `tag:` / `site:` tokens) |
| `↑` / `↓`, `Ctrl-p` / `Ctrl-n` | move the selection |
| `Enter` | connect to the selected host |
| `Ctrl-a` / `Ctrl-e` / `Ctrl-d` | [add / edit / delete](hosts.md) a host |
| `Ctrl-y` | **yank** — copy the generated `ssh` command without connecting |
| `Ctrl-t` | [transfer files](transfer.md) to/from the selected host |
| `Ctrl-f` | [port-forward](port-forwarding.md) through the selected host |
| `Ctrl-o` | [import](import.md) from `~/.ssh/config` (read-only) |
| `F1` | help overlay — every key, in the TUI itself |
| `F2` | settings — hosts file, tmux mode ([Configuration](configuration.md)) |
| `F3` | manage [sites](sites-tags.md) |
| `F4` | manage [port forwards](port-forwarding.md#the-forwards-manager-f4) |
| `Esc` | clear the query if non-empty, otherwise quit |
| `Ctrl-c` | quit |

## What "connect" actually does

`Enter` records the host's usage (for frecency), tears the TUI down, and **`exec`s into
`ssh`** — sshelf is *replaced* by the real ssh process, so there is no wrapper between you
and your session, and when the session ends you're back at your shell. The command it runs is
exactly what `Ctrl-y` (or `sshelf print-command <host>`) shows: plain flags built from the
host's fields plus any inherited [site defaults](sites-tags.md) — no temporary config files.
Full mechanics: [How the ssh command is built](ssh-command.md).

## Connecting inside tmux

By default connecting always hands the terminal over, tmux or not. Set `tmux` to `"window"` or
`"pane"` ([Configuration](configuration.md), or `F2`) and — **when sshelf is itself running
inside tmux** — `Enter` instead opens the connection in a new tmux window (named after the
host) or a new pane, and **leaves you in the picker**. That's the point: fire off four hosts in
a row without reopening sshelf between them. A one-line status confirms each one
(`opened in tmux window: prod-web`).

Outside tmux, or with `tmux = "off"`, nothing changes: `Enter` is exactly the handoff described
above.

### Hosts that always connect in place

Three kinds of connection step back to the normal handoff even in tmux mode, and say so on the
line just before ssh starts:

- **[2FA hosts](passwords-2fa.md#two-factor-2fa-hosts)** — the verification code you typed can
  only reach a new tmux window through `tmux new-window -e KEY=VALUE`, which is the tmux
  client's own command line and therefore visible in `ps`. sshelf will not put a one-time code
  there.
- **Stored-password hosts in [vault mode](passwords-2fa.md#where-secrets-live)** (with
  `$SSHELF_VAULT_PASSPHRASE` set) — same reason: the master passphrase would have to cross the
  same boundary.
- **tmux older than 3.0**, which has no `-e` at all, for hosts with a stored secret.

Everything else — key, agent, and keyring-backed password hosts — opens in tmux normally. Only
the askpass *wiring* crosses (including the host's opaque id, which the helper trades for the
secret); no secret ever does. The reasoning is [D-025](decisions.md).

## Connecting without the TUI

```sh
sshelf prod-web       # connect by name (or id) — same path as Enter
sshelf -              # reconnect to the most recently used host
```

A miss suggests the closest matching names; a host named like a subcommand (`list`,
`import`, …) is reached via the TUI instead. The rest of the CLI:
[CLI reference](cli.md).
