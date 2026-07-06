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
| `F2` | settings ([Configuration](configuration.md)) |
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

## Connecting without the TUI

```sh
sshelf prod-web       # connect by name (or id) — same path as Enter
sshelf -              # reconnect to the most recently used host
```

A miss suggests the closest matching names; a host named like a subcommand (`list`,
`import`, …) is reached via the TUI instead. The rest of the CLI:
[CLI reference](cli.md).
