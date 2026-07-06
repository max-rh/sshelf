# Port forwarding

`Ctrl-f` on a host starts an SSH tunnel that **keeps running after you quit sshelf** — set it
up, close the TUI (or the whole terminal), and it stays up until you stop it or it drops.

## Creating a forward (`Ctrl-f`)

Pick a kind (cycle with `←`/`→`):

- **Local** (`-L`, the default) — a local port that tunnels to something reachable *from the
  server*. E.g. reach the server's private database as `127.0.0.1:8080` on your machine.
- **Remote** (`-R`) — a port *on the server* that tunnels back to something reachable from
  your machine. E.g. let someone on the server's network reach a dev server on your laptop.
- **Dynamic** (`-D`) — a local **SOCKS proxy** that routes traffic through the server.

Fill in the ports/host — defaults: bind `127.0.0.1`, target host `localhost` — and press
`Ctrl-s`. sshelf spawns a detached `ssh -N …` reusing the host's auth exactly as connect does
(keys/agent/ProxyJump, stored password, [site defaults](sites-tags.md)), then waits briefly
to confirm the tunnel actually bound. A failure is shown in the popup so you can fix a field
and retry:

- **local port already in use** — pick another port;
- **privileged port** — ports below 1024 need root; use 1024 or higher;
- **server refused the remote bind** — the server's `sshd` controls remote binds
  (`GatewayPorts`);
- authentication / DNS failures, reported as-is.

On success you're back at the list and the tunnel runs on its own.

## The forwards manager (`F4`)

Lists **every active forward across all hosts** — host, a summary like
`L  127.0.0.1:8080 → db:3306`, pid, and age.

| Key | Action |
|---|---|
| `↑` / `↓`, `Ctrl-p` / `Ctrl-n` | move the selection |
| `d` (or `k`), then `y` | stop the selected forward |
| `Esc` / `Ctrl-s` / `Ctrl-c` | close the manager |

The list refreshes live and is **reconciled against the actually-running processes**: a
forward that ends — stopped here, `kill`ed from another terminal, or dropped on its own after
sleep or network loss — disappears within a moment, and on every launch sshelf shows only
forwards that are still really up. The ledger lives in `forwards.json`
([data model](data-model.md)), but the processes are authoritative — the file is just
remembered PIDs. Design details: [`decisions.md`](decisions.md), D-021.

## Why they survive

Each forward is its own detached process in its own process group, with no tie to sshelf or
your terminal: quitting sshelf orphans it (fine), and closing the terminal doesn't hang it
up. Stop one from `F4` — or `kill <pid>` works too; sshelf notices either way.
