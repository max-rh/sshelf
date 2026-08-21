# Architecture

`sshelf` is a single binary that runs in one of two modes:

1. **Interactive TUI** (default, and subcommands like `import`) — the fuzzy launcher.
2. **Askpass helper** (`SSHELF_ASKPASS=1` in the environment) — a headless, non-interactive
   mode that `ssh` invokes to obtain a stored password. Never run directly by the user.

## High-level flow

```
                 ┌─────────────────────────────────────────────┐
                 │                  sshelf (TUI)                 │
                 │                                               │
   hosts.toml ──▶│  store ──▶ model(Host) ──▶ search (fuzzy +    │
   state.json ──▶│            frecency) ──▶ ui (list/wizard)     │
   config.toml ─▶│                                               │
                 │     user presses Enter on a host ──┐          │
                 └────────────────────────────────────┼──────────┘
                                                       │
              1. update frecency (use_count, last_used) & save
              2. set env: SSH_ASKPASS=self, SSH_ASKPASS_REQUIRE=force,
                          SSHELF_ASKPASS=1, SSHELF_HOST_ID=<id>
              3. tear down TUI (raw mode off, leave alt screen, show cursor)
              4. exec("ssh", argv…)   ← process is REPLACED; sshelf is gone
                                                       │
                                                       ▼
                 ┌─────────────────────────────────────────────┐
                 │                     ssh                       │
                 │  needs a password? ─▶ runs SSH_ASKPASS:       │
                 │     `sshelf "<prompt>"`  (SSHELF_ASKPASS=1)   │
                 │              │                                │
                 │              ▼                                │
                 │   askpass mode: inspect argv[1];              │
                 │   if password prompt ─▶ secrets.get(host_id)  │
                 │      (keyring → age vault) ─▶ print, exit 0    │
                 │   else ─▶ exit non-zero (decline)             │
                 └─────────────────────────────────────────────┘
                                                       │
                                          interactive ssh session
                                                       │
                                              session ends → back at shell
```

## Why this shape

- **`exec()` (process replacement), not spawn+wait.** The user chose *exit-to-shell*: when
  the SSH session ends, they're back at their normal prompt. `exec()` gives `ssh` the real
  TTY with zero indirection — the cleanest possible handoff. Consequence: **no code runs
  after `exec()`**, so anything that must persist (frecency) happens *before* it.

- **One exception: tmux mode.** With `tmux = "window"`/`"pane"` and `$TMUX` set, connect spawns
  `tmux new-window`/`split-window` instead and sshelf stays up, so several hosts can be opened
  in a row. Frecency is persisted before the spawn for the same reason. Connections whose
  authentication would have to cross tmux's argv (a 2FA code, a vault passphrase) fall back to
  the `exec()` path — see [`security.md`](./security.md) and D-025.

- **Password auto-supply via `SSH_ASKPASS`, not `sshpass`.** `ssh` never accepts a password
  on the command line; `sshpass` would expose it in `ps`/argv and is an extra dependency.
  `SSH_ASKPASS` (OpenSSH 8.4+) lets `ssh` call a helper program for the password. We point
  it at our own binary. With `SSH_ASKPASS_REQUIRE=force`, ssh uses the helper even though a
  TTY is present. See [`ssh-command.md`](./ssh-command.md) for the full mechanism and its
  sharp edges (the helper must inspect the prompt; host-key prompts must be neutralized).

- **Two-tier secrets.** OS keyring (macOS Keychain / Linux Secret Service) is the primary
  store. Headless/minimal Linux often has no Secret Service daemon, so an `age`-encrypted
  vault (unlocked by a master passphrase, cached in-memory per session) is the fallback. See
  [`security.md`](./security.md).

- **Own database, never `~/.ssh/config`.** Hosts live in `hosts.toml` (human-readable, atomic
  writes). Importers (`~/.ssh/config`, a Tailscale tailnet) are read-only towards their source
  and add-only towards the database. See [`data-model.md`](./data-model.md).

- **Synchronous event loop, component pattern.** No background work needs multiplexing (the
  one long-running thing, the SSH session, happens *after* the TUI is gone). A simple
  `crossterm::event::read()` loop with a component-per-screen structure (HostList, Wizard,
  Help, Confirm) keeps it small and tokio-free.

## Component map

See [`structure.md`](./structure.md) for the file-by-file breakdown. At runtime:

- `App` owns top-level state (current screen, query, selection, loaded hosts + state) and
  routes events to the active component.
- `search` turns `(hosts, state, query)` into a ranked, highlight-annotated view.
- `ssh` is the only place that builds argv and performs the teardown + `exec()`.
- `secrets` is the only place that talks to the keyring/vault; both the TUI (to *store* on
  add/edit) and the askpass mode (to *retrieve*) go through it.

## Failure handling

- If `exec()` returns, it failed (e.g. `ssh` not found) → restore the TUI and surface the error.
- If the askpass helper can't get the secret → exit non-zero so `ssh` falls back to prompting
  the user, rather than hanging or sending a wrong answer.
- A panic mid-TUI restores the terminal via a guard + panic hook before unwinding.
