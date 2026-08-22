# `sshelf doctor`

```sh
sshelf doctor
```

One command that checks the things that quietly break connections — the OpenSSH version, the
secret backend, a site that no longer exists, a stale export, a missing agent — and tells you,
per check, what to do about it. If something isn't working, run this first.

```
sshelf doctor — checking this machine and your host database

  ok    ssh — OpenSSH 9.8 (8.4+, so stored secrets can be supplied)
  ok    host database — 12 host(s), 2 site(s) in ~/.config/sshelf/hosts.toml
  ok    secrets — the OS keyring is reachable
  fail  sites — 1 host(s) point at a site that isn't defined: web → "prod-dc"
        → Define it with `sshelf sites add prod-dc`, or clear the field with Ctrl-e — until
          then those hosts inherit nothing from it.
  ok    orphaned secrets — not checked (the OS keyring can't be listed; vault mode can)
  warn  ssh-agent — $SSH_AUTH_SOCK is not set, but 3 host(s) use agent auth: web, db, edge
        → Start an agent (`eval $(ssh-agent)`, or your desktop's) and `ssh-add` your key —
          otherwise those hosts fall back to whatever else ssh can find.
  ok    ssh_config export — up to date (~/.config/sshelf/ssh_config)

1 failed, 1 warning(s), 5 ok
```

Every line is `ok`, `warn`, or `fail`, and anything that isn't `ok` carries **one runnable next
action** on its own line.

- **`warn`** — it works today, but it limits something or will bite later.
- **`fail`** — it's broken now.

**Exit code:** `0` when nothing failed (warnings are fine), `1` if any check failed. So
`sshelf doctor && deploy.sh` does what you'd expect, and CI can gate on it.

## What it checks

| Check | Fails when | Warns when |
|---|---|---|
| **ssh** | `ssh -V` reports older than OpenSSH 8.4, or `ssh` can't be run | the version string can't be parsed |
| **host database** | `hosts.toml` doesn't parse, or two hosts share a name or an id | — |
| **secrets** | the configured backend can't be used (no keyring service; an unreadable vault) | — |
| **sites** | a host's `site` names a site that isn't defined | — |
| **orphaned secrets** | — | a stored secret belongs to no host (vault mode only — see below) |
| **ssh-agent** | — | `$SSH_AUTH_SOCK` is unset, or points at a socket that's gone, while hosts use agent auth |
| **ssh_config export** | — | the [exported fragment](export.md) no longer matches your hosts |

Two notes on the edges:

- **Duplicate ids are a failure, not a nitpick.** The id keys both the stored secret and the
  frecency history, so two hosts sharing one share a password and a usage count.
- **Orphaned secrets can only be found in vault mode.** The OS keyring offers no portable way to
  list what's in it, so the check says it didn't run rather than reporting a clean bill of
  health nobody checked. In [vault mode](passwords-2fa.md#where-secrets-live) the vault is a map
  sshelf owns, so the ids are right there.

## What it does *not* do

- **It never contacts a host.** No pings, no test connections, no version checks over the
  network. sshelf makes no network calls of its own ([Security](security.md)) and `doctor` is no
  exception — it tells you whether *sshelf* is set up, never whether a *server* is up.
- **It never changes anything.** Every check is a read, with exactly one exception: in keyring
  mode it writes a clearly-named throwaway entry and deletes it again, because a backend you can
  read but not write is a backend that fails the first time you save a password.
- **It doesn't fix anything for you.** Each not-ok line names the command or the key to press;
  you decide.
- **No `--json`.** Human-readable output only for now — see [D-027](decisions.md).

## When to reach for it

- A password host prompts you instead of connecting → the **ssh** and **secrets** checks.
- A host connects but ignores its site's bastion or user → the **sites** check.
- `ssh <name>` works from sshelf but not from your shell → the **export** check.
- Key auth suddenly asks for a passphrase → the **ssh-agent** check.
- Anything at all after hand-editing `hosts.toml` → the **host database** check.
