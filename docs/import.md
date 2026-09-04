# Importing hosts

sshelf can populate its database from two sources you already have: your **SSH config** and
your **Tailscale tailnet**. Both are read-only towards their source and **add-only** towards
sshelf: a host whose name already exists is left alone, so re-running is always safe.

## From `~/.ssh/config`

`sshelf import` (or `Ctrl-o` in the TUI) copies hosts from `~/.ssh/config` into sshelf's own
database. It is **strictly read-only**: sshelf parses the file and never writes back. Your
SSH config is not touched, ever.

```sh
sshelf import --dry-run    # preview what would be imported
sshelf import              # import
```

What it does:

- adds every host whose **name isn't already present** in sshelf, so re-running is safe and
  existing names are left alone;
- carries over the fields sshelf models (hostname, user, port, identity files);
- warns about the directives it doesn't import (`Match`, `Include`, `ProxyJump`) instead of
  silently mis-importing them.

## From Tailscale

`sshelf import --tailscale` imports your **tailnet**: every machine in it becomes a
searchable sshelf host, in one command.

```sh
sshelf import --tailscale --dry-run   # preview
sshelf import --tailscale             # import
```

It runs **your own** `tailscale` CLI (`tailscale status --json`) and reads the output. sshelf
still makes no network calls of its own: nothing happens unless you run this command, never
at startup, never on a save, never in the background, and there's no Tailscale entry point in
the TUI. Your API keys and tailnet credentials stay with the Tailscale client; sshelf never
sees them, and nothing tailscale-specific is written to `hosts.toml`.

**Which machines are imported.** Every peer whose MagicDNS name is under your tailnet's own
domain, one rule that leaves out Mullvad exit nodes and machines shared in from other
tailnets. Peers with an **expired** node key are skipped. **Offline peers are imported**: being
asleep is temporary, and the host is still real. This machine (`Self`) is not imported.

**What each machine becomes:**

| sshelf field | From the peer |
|---|---|
| `name` | First label of the MagicDNS name, lowercased (e.g. `nas.tail4f9a2.ts.net.` → `nas`). |
| `hostname` | The MagicDNS FQDN (`nas.tail4f9a2.ts.net`), stable across IP churn, and what Tailscale SSH expects. If MagicDNS is off for your tailnet, its Tailscale IP (IPv4 first). |
| `site` | Your tailnet's name, matched to an existing [site](sites-tags.md) case-insensitively, or created as a bare one (name only, no defaults). |
| `tags` | The machine's ACL tags, minus the `tag:` prefix (`tag:server` → `server`). Untagged machines get no tags. |
| `auth` | `agent`, your key/agent auth, unchanged. Add a user, port or password afterwards if a box needs one. |

Two machines whose names collide (possible only across sub-domains) keep the first; the second
is reported. Everything skipped is counted in a warning line, so the numbers always add up.

**If sshelf can't find the CLI.** It looks at `$SSHELF_TAILSCALE_BIN`, then `tailscale` on your
`PATH`, then `/Applications/Tailscale.app/Contents/MacOS/Tailscale`, since the macOS app doesn't put
its CLI on `PATH`. For any other location:

```sh
SSHELF_TAILSCALE_BIN=/path/to/tailscale sshelf import --tailscale
```

The import needs the Tailscale backend to be **running** (`tailscale up`); if it isn't, sshelf
says so and changes nothing.

## After importing

Import brings everything in at once (there's no per-host picker); curate afterwards with
`Ctrl-e` / `Ctrl-d`, and organize with tags or [sites](sites-tags.md). Your `~/.ssh/config`
keeps working exactly as before, and sshelf's database is independent of it by design (the
[FAQ](faq.md#why-doesnt-sshelf-just-use-my-ssh-config) explains why).

The reverse direction exists too: [**export**](export.md) projects your sshelf hosts back out
as an `Include` fragment, so plain `ssh`/`scp` can use them by name, including everything a
tailnet import just added.
