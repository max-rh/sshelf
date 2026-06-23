# Project structure

> Keep this in sync with the actual tree (the docs-in-sync rule).
>
> **All modules present:** `main`, `app`, `askpass`, `config`, `forwards`, `import`, `model`,
> `paths`, `search`, `secrets`, `ssh`, `state`, `store`, `transfer/{mod,worker,pane,screen,e2e}`,
> `testsupport` (test-only), `vault`,
> `ui/{mod,list,help,widgets,wizard,browse,settings,sites,transfer,forward_popup,forwards,two_factor}`.
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
| `model.rs` | `Host` + `Site` structs (+ `AuthMethod`); `Host::with_site_defaults`/`find_site` (site inheritance); serde derives. |
| `store.rs` | Load/save `hosts.toml` with atomic write (temp + rename); load `config.toml`. |
| `state.rs` | Frecency state (`use_count`, `last_used`) load/save (`state.json`); score computation. |
| `forwards.rs` | Background port-forwards: the `ForwardSpec`/`ForwardEntry` model, the `-L/-R/-D` argv builder, spawn (detached `ssh -N` + readiness/error mapping), PID liveness/kill via `ps`/`kill`, reconcile, and `forwards.json` load/save. |
| `secrets.rs` | `SecretStore` trait → keyring backend + `age`-vault fallback; `zeroize` on secrets. |
| `ssh.rs` | Build `ssh` argv from a `Host`; terminal teardown + `exec()` handoff; askpass env wiring. |
| `askpass.rs` | Headless askpass entry: inspect `argv[1]`; answer password prompts via `secrets`, or a queued 2FA code (`SSHELF_2FA_CODE`) for the verification prompt; else decline. |
| `search.rs` | Fuzzy filter (`nucleo-matcher`) + frecency ranking + per-row match indices for highlight. |
| `import.rs` | `ssh2-config` parse of `~/.ssh/config` → `Host` mapping; warn on unsupported `Match`/`Include`. |
| `paths.rs` | `etcetera` path resolution (config/data dirs); file paths; dir/file perms (`0700`/`0600`). |
| `config.rs` | Preferences: `decay_rate`, `default_sort`, `accent` color; writes a commented default on first run. |
| `transfer/mod.rs` | File-transfer core: `ssh`-ControlMaster + `sftp` argv builders, the worker↔UI message protocol (`WorkerCmd`/`WorkerEvent`), and progress math. |
| `transfer/worker.rs` | Background worker thread: owns the ControlMaster (open/readiness/teardown), lists remote dirs (`sftp ls -l`), runs `sftp` `get`/`put` transfers with progress + cancel. |
| `transfer/pane.rs` | One side's browsing state (fuzzy filter + selection + nav, reusing `search`); `read_local_dir` for the local side; `RemoteEntry`→`PaneEntry`. |
| `transfer/screen.rs` | The dual-pane `TransferScreen`: two panes over one session, key handling, draining worker events. |
| `ui/list.rs` | Host list rendering + match highlighting + selection. |
| `ui/transfer.rs` | Renders the transfer screen (two panes + progress/status + hint bar) from a borrowed view. |
| `ui/wizard.rs` | Auth-aware add/edit form: fields, validation, key picker, opens the file browser. |
| `ui/browse.rs` | File-browser modal (fuzzy-filtered) for picking a key file anywhere on disk. |
| `ui/settings.rs` | Settings screen (F2): config-file display + editable hosts-file location. |
| `ui/sites.rs` | Sites manager (F3): list + add/edit/delete sites and their optional defaults; emits renames for the app to cascade. |
| `ui/forward_popup.rs` | New-port-forward popup (Ctrl-f): kind chooser (Local/Remote/Dynamic) + ports/host fields + validation; emits a `ForwardSpec` for the app to spawn. |
| `ui/forwards.rs` | Port-forwards manager (F4): lists all active forwards from a live snapshot; emits a kill request for the app to act on. |
| `ui/two_factor.rs` | 2FA code popup shown before connecting to a `requires_2fa` host; emits the entered code for the app to queue + supply via askpass. |
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
