# Data model & on-disk layout

## File locations

Paths resolve via the `etcetera` **base strategy** (XDG everywhere — `~/.config/sshelf` on
both macOS *and* Linux, honoring `XDG_CONFIG_HOME`/`XDG_DATA_HOME` when set). This keeps
config hand-editable instead of buried in macOS `~/Library`.

| File | Location (default) | Owner | Purpose |
|---|---|---|---|
| `hosts.toml` | `~/.config/sshelf/hosts.toml` | user | The host database. Human-editable. |
| `config.toml` | `~/.config/sshelf/config.toml` | user | Preferences (theme, `decay_rate`, sort, keybinds). |
| `state.json` | `~/.local/share/sshelf/state.json` | app | Frecency counters, keyed by host id. Churns; not for hand-editing. |
| `forwards.json` | `~/.local/share/sshelf/forwards.json` | app | Ledger of active background port-forwards (PIDs). Reconciled against the OS on launch. Mode `0600`. |
| `ssh_config` | `~/.config/sshelf/ssh_config` | app | Exported ssh_config `Include` fragment (`sshelf export`) — derived from `hosts.toml`, refreshed on every hosts save once present. Mode `0600`. |
| `vault.age` | `~/.local/share/sshelf/vault.age` | app | **Fallback** encrypted secret store (only when no OS keyring). Mode `0600`. |

Directories are created on first run (`0700`). **Secrets are never written to `hosts.toml`.**

## `Host` / `Site` schema (`hosts.toml`)

```toml
format_version = 1            # top-level scalar; for future migrations

[[site]]                      # optional; sites are listed before hosts (scalars-before-AoT)
name      = "prod-dc"         # the name hosts reference (see host.site)
user      = "deploy"          # optional default login for member hosts
port      = 22                # optional default port
jump_hosts = ["bastion.prod"] # optional default ProxyJump (the site's bastion)
identity_files = ["~/.ssh/prod"]  # optional default key(s) (applied to key-auth members)

[[host]]
id        = "01J…"            # stable unique id (e.g. ULID/UUID); keys secrets & frecency
name      = "prod-db"         # display alias (what you search/see)
hostname  = "10.25.25.25"     # IP or DNS name           (required)
user      = "mike"            # optional; default = $USER at connect time
port      = 22                # optional; default 22
auth      = "key"             # "key" | "password" | "agent"
identity_files = ["~/.ssh/infra-key"]   # for auth="key"; repeatable (-i per entry)
jump_hosts = ["bastion.example.com"]    # ProxyJump chain; key/agent auth only in v1
tags      = ["prod", "db"]    # many-valued, free-form; for filtering/grouping
site      = "prod-dc"         # optional; one site (by name); groups + inherits its defaults
requires_2fa = true           # optional (default false); connect prompts for a verification code
extra_args = "-o ServerAliveInterval=30"  # raw, shlex-split, appended verbatim
# NOTE: no password field — ever. auth="password" means "look up the secret by id".
```

Notes:
- Optional fields use `Option<T>` in Rust with `#[serde(skip_serializing_if = "Option::is_none")]`
  so the TOML stays clean; new fields use `#[serde(default)]` for backward compatibility.
- `identity_files` / `jump_hosts` / `tags` are `Vec<String>` (empty = absent).
- `format_version` lets us migrate the schema later without breaking older files. Adding `[[site]]`
  and `host.site` needed **no** bump — old files load with `sites = []` / `site = None`.
- `requires_2fa` marks a host whose login needs an interactive verification code; connect collects
  it and passes it to `ssh` via the transient `SSHELF_2FA_CODE` env var (never stored on disk).
  See [`decisions.md`](./decisions.md) D-022.

### Sites vs tags, and inheritance

A **Site** is *one per host* and may carry optional shared SSH defaults; **tags** are
many-valued free-form labels. At connect time a host is resolved into an *effective host*
(`Host::with_site_defaults`): for `user`, `port`, `jump_hosts`, `identity_files`, the site's
value fills in **only where the host leaves that field unset** — the host always wins. Auth is
**not** inheritable. A host that names an **undefined** site still groups under that name but
inherits nothing (graceful degradation). Renaming a site (F3 manager) cascades to member hosts;
deleting one clears members' `site`. See [`decisions.md`](./decisions.md) D-020.

## Frecency state (`state.json`)

```json
{
  "01J…": { "use_count": 12, "last_used": "2026-06-05T09:30:00Z" }
}
```

- Keyed by host `id` (so renaming a host in `hosts.toml` keeps its history).
- Updated **before** `exec()` on connect: `use_count += 1`, `last_used = now`.
- Kept separate from `hosts.toml` so the user-owned host file stays stable and diff-friendly.
- Score: `use_count * exp(-decay_rate * days_since_last_used)` (`decay_rate` default `0.2`).
  See [`ux.md`](./ux.md) for how it combines with fuzzy ranking.

## Port-forward ledger (`forwards.json`)

```json
[
  {
    "id": "01J…",                       // ULID; also names the forward's stderr log
    "host_id": "01J…",                  // originating host id
    "host_name": "prod-db",             // snapshot for display
    "kind": "local",                    // "local" | "remote" | "dynamic"
    "spec": { "listen_port": 8080, "target_host": "db", "target_port": 3306 },
    "display": "L  127.0.0.1:8080 → db:3306",
    "pid": 41234,                       // the detached `ssh -N` process
    "started_at": 1718900000
  }
]
```

- App-owned; written atomically (`0600`). The running `ssh` processes are authoritative — this
  file is only a remembered list of PIDs, **reconciled** against the OS (`ps`) on startup, on
  opening the manager, and each tick while it's open. A forward leaves the ledger the moment its
  process is gone, however it ended. See [`decisions.md`](./decisions.md) D-021.
- `spec` omits empty fields (`bind` defaults to `127.0.0.1`, `target_host` to `localhost`);
  Dynamic forwards carry only `listen_port`.

## Secrets

Stored in the OS keyring (service `sshelf`, account = host `id`) or, as a fallback on
headless systems, in `vault.age`. Either way the key is the host `id`. Full model and threat
analysis in [`security.md`](./security.md).

## Atomic writes

All persistent writes use temp-file + `rename()` (atomic on Unix) so a crash mid-write never
corrupts `hosts.toml` / `config.toml` / `state.json`. Single-process tool → no file locking needed.
