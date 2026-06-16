# Project structure

> Keep this in sync with the actual tree (the docs-in-sync rule).
>
> **All modules present:** `main`, `app`, `askpass`, `config`, `import`, `model`, `paths`,
> `search`, `secrets`, `ssh`, `state`, `store`, `transfer`, `vault`,
> `ui/{mod,list,help,widgets,wizard,browse,settings}`.
> (`error.rs` was removed — the codebase uses `anyhow` throughout.)

## Repository

```
ssh-tui/                 (crate/binary name: `sshelf`)
├── Cargo.toml
├── CONTRIBUTING.md      contributor guide + docs-in-sync rule
├── README.md           (M8) user-facing intro + positioning
├── SECURITY.md         (M8) threat model for OSS users (mirrors docs/security.md)
├── LICENSE-MIT         (M1)
├── LICENSE-APACHE      (M1)
├── docs/               living documentation (this directory)
└── src/                see below
```

## `src/` modules

| File | Responsibility |
|---|---|
| `main.rs` | Entry/dispatch. If `SSHELF_ASKPASS` is set → askpass mode (read `argv[1]`). Else clap parses: default TUI, or subcommands (`import`, `list`, `add`). |
| `app.rs` | `App` state + synchronous event loop + screen routing (component orchestration). |
| `model.rs` | `Host` struct + `AuthMethod` enum (`Key` / `Password` / `Agent`); serde derives. |
| `store.rs` | Load/save `hosts.toml` with atomic write (temp + rename); load `config.toml`. |
| `state.rs` | Frecency state (`use_count`, `last_used`) load/save (`state.json`); score computation. |
| `secrets.rs` | `SecretStore` trait → keyring backend + `age`-vault fallback; `zeroize` on secrets. |
| `ssh.rs` | Build `ssh` argv from a `Host`; terminal teardown + `exec()` handoff; askpass env wiring. |
| `askpass.rs` | Headless askpass entry: inspect `argv[1]`; answer only password prompts via `secrets`. |
| `search.rs` | Fuzzy filter (`nucleo-matcher`) + frecency ranking + per-row match indices for highlight. |
| `import.rs` | `ssh2-config` parse of `~/.ssh/config` → `Host` mapping; warn on unsupported `Match`/`Include`. |
| `paths.rs` | `etcetera` path resolution (config/data dirs); file paths; dir/file perms (`0700`/`0600`). |
| `config.rs` | Preferences: `decay_rate`, `default_sort`, `accent` color; writes a commented default on first run. |
| `transfer/mod.rs` | File-transfer core: `ssh`-ControlMaster + `sftp`/`scp` argv builders, the worker↔UI message protocol, and progress math. (Worker thread + dual-pane UI land with the transfer screen.) |
| `ui/list.rs` | Host list rendering + match highlighting + selection. |
| `ui/wizard.rs` | Auth-aware add/edit form: fields, validation, key picker, opens the file browser. |
| `ui/browse.rs` | File-browser modal (fuzzy-filtered) for picking a key file anywhere on disk. |
| `ui/settings.rs` | Settings screen (F2): config-file display + editable hosts-file location. |
| `ui/help.rs` | Help overlay. |
| `ui/widgets.rs` | Shared widgets: single-line text input (hand-rolled), keybind hint bar, confirm modal. |

## Data flow at a glance

```
paths ──▶ store ──▶ model(Host[])  ┐
paths ──▶ state ──▶ frecency       ├─▶ search ──▶ ui ──▶ (Enter) ──▶ ssh ──▶ exec
                                   ┘                                  │
secrets ◀── ui (store on add/edit)                     askpass ◀── ssh (via SSH_ASKPASS)
   └────────────────────────────────────────────────────▶ secrets (retrieve)
```

## Conventions

- One responsibility per module; `ssh.rs` is the *only* place that calls `exec()`; `secrets.rs`
  is the *only* place that touches the keyring/vault.
- No `unwrap()`/`expect()` on fallible I/O in non-test code — return errors, surface them in the UI.
- Every new/moved module updates this file.
