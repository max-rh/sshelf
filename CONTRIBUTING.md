# Contributing to `sshelf`

Guidance for anyone working in this repo. Read this first.

`sshelf` is a terminal UI (TUI) for managing and connecting to SSH hosts. You save each
node once (via a guided wizard), then fuzzy-search and connect with `Enter`. It **generates
and runs the correct `ssh` command** from its own host database — it does **not** read or
edit your `~/.ssh/config` (except an explicit, read-only *import*).

> Working dir is `ssh-tui/`; the crate/binary is **`sshelf`**. The folder may be renamed to
> `sshelf/` later — it has no effect on the build.

---

## The docs-in-sync rule

**Every change to code or behavior MUST update the relevant doc(s) under `docs/` in the
same change, and add a dated entry to `docs/progress.md`.**

- Docs are the source of truth. Code and docs must never drift.
- Touched `ssh.rs`/argv logic → update `docs/ssh-command.md`.
- Changed the data schema or file locations → update `docs/data-model.md`.
- Added/moved a module → update `docs/structure.md`.
- Changed user-facing behavior (a keybinding, screen, CLI command, config key) → update the
  relevant **Guide** page under `docs/` (install, quickstart, hosts, search-connect, transfer,
  port-forwarding, sites-tags, passwords-2fa, import, cli, configuration, faq); UI design
  rationale lives in `docs/ux.md`.
- Made a non-trivial design choice → add an entry to `docs/decisions.md`.
- Anything security-relevant (secrets, askpass, perms) → update `docs/security.md`.
- **Always** append what you did + what's next to `docs/progress.md`.

If a change doesn't fit any doc, it still gets a `docs/progress.md` line. No silent changes.

---

## Status

v1 feature-complete (M0–M8): fuzzy list + connect (`exec` handoff), add/edit/delete form,
secrets + password auto-supply (keyring/vault + askpass), tags, frecency, config, and
read-only `~/.ssh/config` import. Builds + tests pass on macOS and Linux; clippy clean.
See `docs/progress.md` for status and the **unverified-paths acceptance gates** (macOS
keyring on a real GUI session, the interactive TTY→exec handoff, first CI run).

## Product constraints (non-negotiable)

- **Never mutate `~/.ssh/config`.** Own database only. Import is strictly read-only.
- **Store & auto-supply passwords**, cross-platform (macOS + Linux). Open-source.
- **Exit-to-shell on connect**: tear down the TUI and `exec()` into `ssh`; when the session
  ends the user is back at their normal shell.
- atuin.sh aesthetic: slim chrome, inline fuzzy-filter list, bottom keybind hints.

## Tech stack & toolchain

- **Rust** (edition 2024) + **ratatui** + **crossterm**. Synchronous event loop (no tokio).
- **Toolchain: Rust 1.88+ is REQUIRED** (ratatui 0.30 MSRV). Run `rustup update` before building.
- Key crates: `nucleo-matcher` (fuzzy), `serde`+`toml` (host DB/config), `etcetera` (XDG
  paths), `keyring` + `age` + `zeroize` (secrets), `clap` (CLI), `ssh2-config` (import),
  `shlex`, `unicode-width`, `time`, `anyhow`/`thiserror`. See `docs/structure.md` for the
  full pinned list.

## Build / run / test

```sh
rustup update                 # ensure >= 1.88
cargo build
cargo clippy -- -D warnings
cargo test
cargo run                     # launches the TUI
cargo run -- import           # read-only import from ~/.ssh/config (M7)
echo "$PASS" | cargo run -- set-password <name>   # store a password (headless provisioning)
cargo run -- --config FILE    # use a specific config file (also: $SSHELF_CONFIG)
cargo run -- completions zsh  # shell completions (bash/zsh/fish/…); `man` prints the man page
# Distribution: dist (Homebrew + tarballs) + cargo-deb (.deb) — see docs/packaging.md
# askpass mode is NOT user-facing — ssh invokes `sshelf "<prompt>"` with SSHELF_ASKPASS=1
# (see docs/ssh-command.md). Secret backend: OS keyring by default, age vault if
# SSHELF_VAULT_PASSPHRASE is set.
```

## Hard conventions / invariants (do not violate)

1. **No secrets in `hosts.toml`.** Passwords live only in the OS keyring or the `age` vault,
   keyed by host id. `hosts.toml` stores at most `auth = "password"`.
2. **Config location** comes from `--config` / `$SSHELF_CONFIG` (else XDG); the **hosts-file**
   location is the `hosts_file` config key (default under the config dir), editable via F2
   settings. The config file's *own* location can't be a config key (bootstrap), so it's
   flag/env only. The vault/state stay in the XDG data dir regardless (so askpass is
   unaffected by a custom config).
2. **Update frecency state BEFORE `exec()`.** `exec()` replaces the process — no code runs
   after it. Persist `use_count`/`last_used` first, then hand off.
3. **The askpass helper MUST inspect `argv[1]`** (the prompt text) and answer *only* password
   prompts. With `SSH_ASKPASS_REQUIRE=force`, ssh routes the host-key `yes/no` prompt to the
   helper too — answering it with the password breaks the connection. Also pass
   `-o StrictHostKeyChecking=accept-new` so that prompt normally never fires.
4. **Askpass mode is detected via the `SSHELF_ASKPASS=1` env var, not a CLI flag** (ssh calls
   the helper as `sshelf "<prompt>"`).
5. **Restore the terminal on every exit path**, including panic — use a RAII guard + panic hook.
6. **Jump hosts must use key/agent auth in v1** (password-auth jump hosts are unsupported;
   the one-secret-per-target model can't disambiguate hops).
7. **Platforms: macOS + Linux only** for v1 (`exec()` replacement is Unix-only).

## Where things live

- `docs/` — the living documentation (see `docs/index.md`).
