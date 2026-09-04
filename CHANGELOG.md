# Changelog

Notable, user-facing changes per release. Follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow SemVer.

## [Unreleased]

### Fixed
- **The host list now shows the user a host inherits from its site**, instead of falling back
  to your local `$USER`. Only the display was ever wrong: connecting, `Ctrl-y` and
  `sshelf print-command` already used the site's user. The fix covers the TUI rows,
  `sshelf list` and shell-completion help, and searching for that user now matches the hosts
  that inherit it. `sshelf list --json` is unchanged, and still reports the record as stored,
  so an inherited `user` stays `null` with the resolved values in the generated `command`.
  (#16)
- **The remote pane of the transfer screen now lists hidden files**, as the local pane always
  did, so you can open `.config` on a server and copy things out of it. sshelf lists over SFTP
  with `ls -la`; `.` and `..` still never appear on either side. There is no show/hide toggle,
  and typing a `.` into the pane filter keeps the names that have one. (#15)

### Changed
- **README rewritten as a landing page** — a re-recorded demo clip, one section and one real
  screenshot per feature (`docs/assets/`), why the project exists, a comparison with the tools
  people weigh it against, and the never-list stated as promises. New **`PRIVACY.md`** at the
  repo root (what sshelf reads, writes, runs, and sends) and **`docs/llms.txt`**. Docs and
  assets only — no behavior change.

## [0.13.0] — 2026-08-21

### Added
- **`sshelf doctor`** — one command that checks the things that quietly break connections and
  names the fix for each: the OpenSSH version (8.4+ is what stored passwords ride on), whether
  your secret backend actually opens, whether `hosts.toml` parses and has no duplicate names or
  ids, hosts pointing at a site that no longer exists, stored secrets whose host is gone, a
  missing or stale `$SSH_AUTH_SOCK` when hosts use agent auth, and an exported ssh_config
  fragment that has drifted from your hosts. Each line is `ok` / `warn` / `fail` with one
  runnable next action; it **exits 1** if anything failed, so `sshelf doctor && …` works in a
  script. Local and read-only throughout — it never contacts a host, and the only write is a
  throwaway keyring entry it deletes again. Full page:
  [Checking your setup](https://max-rh.github.io/sshelf/doctor.html). No new dependencies.

### Fixed
- **Global flags now work before a subcommand.** `sshelf --config FILE list` used to be read as
  "connect to a host named `list`", and `sshelf --config FILE set-password web` failed to parse
  at all — despite `--config` being documented as global. Both now work, on either side of the
  subcommand, as does `--transfer-log`. (Combining a host name *and* a subcommand —
  `sshelf prod-web list` — is now refused explicitly instead of silently running the subcommand
  and dropping the host.)

### Changed
- **Every user-facing error now names the thing, the cause, and the next action.** A failed save
  names the file it couldn't write instead of just "save failed"; an unknown host, a duplicate
  site, and an empty `--password-stdin` each say what to run instead; a failed `ssh` launch asks
  whether OpenSSH is on your `PATH`; a forward that can't authenticate says what to check; an
  undecryptable vault names the environment variable to fix.
- The FAQ answers that used to end in a shrug now end in `sshelf doctor`.

## [0.12.0] — 2026-08-21

### Added
- **tmux mode** — set `tmux = "window"` or `"pane"` (in `config.toml`, or on the `F2` settings
  screen) and, when sshelf is running inside tmux, `Enter` opens the host in a new tmux window
  (named after it) or a new pane and **leaves you in the picker**, so you can fire off several
  connections in a row. Outside tmux, or with the default `"off"`, connecting is unchanged:
  tear down, hand the terminal to `ssh`, exit to your shell. Frecency is recorded before the
  window opens, exactly as before the `exec()`. Hosts whose authentication would have to travel
  through tmux's command line — a
  [2FA](https://max-rh.github.io/sshelf/passwords-2fa.html#two-factor-2fa-hosts) verification
  code, or a stored secret in vault mode — connect in place instead and say why; only the
  askpass wiring (never a secret) is ever passed to tmux. No new dependencies: sshelf runs your
  own `tmux` binary.
- **Transfer: mark several and send them at once** — `Space` marks the file or folder under the
  cursor, `Ctrl-a` marks everything the filter shows (again to clear), and `Ctrl-s` sends the
  whole set — folders recursively — through the one authenticated connection, counting through
  the batch. An entry the destination already has is skipped and the queue carries on; the
  summary names what was passed over. `Esc` now clears marks before clearing the filter.
- **Transfer: create directories with `F7`** (or `Ctrl-f`) on either side — a one-line input at
  the bottom of the focused pane. It creates exactly one directory in that pane's current
  directory, never adopts an existing name, and puts the new directory under the cursor.

### Fixed
- Cancelling a transfer with `Esc` left the transfer screen stuck in its "transfer running"
  state, ignoring every key but `Esc` and `Ctrl-c`. It now reports the cancellation and returns
  to browsing.

### Changed
- The `F1` help overlay documents the transfer screen's keys and the active tmux mode.
- README, FAQ, and the docs site point at
  [GitHub Discussions](https://github.com/max-rh/sshelf/discussions) for questions and feature
  requests.

## [0.11.0] — 2026-07-27

### Added
- **`sshelf import --tailscale`** — import your Tailscale tailnet: every eligible machine
  becomes a searchable sshelf host in one command. Runs **your own** `tailscale` CLI
  (`tailscale status --json`) and maps each peer's MagicDNS name to the host name, its FQDN to
  the hostname (stable across IP churn; the Tailscale IP when MagicDNS is off), your tailnet to
  a [site](https://max-rh.github.io/sshelf/sites-tags.html), and its ACL tags to sshelf tags.
  Mullvad exit nodes and machines shared in from other tailnets are left out, expired nodes are
  skipped, and offline machines are imported (being asleep is temporary). **Add-only:** hosts
  and sites that already exist are never touched, so re-running adds 0 — and, as ever, nothing
  under `~/.ssh` is written. sshelf still makes no network calls of its own: the CLI runs only
  when you type the command, never at startup or in the background. `$SSHELF_TAILSCALE_BIN`
  points at the binary if it isn't on your `PATH` (the macOS app doesn't add it). No new
  dependencies.

### Changed
- `sshelf import` now reports how many parsed hosts were skipped as duplicates, and prints any
  site it creates.

## [0.10.0] — 2026-07-12

### Added
- **`sshelf export`** — project the host database as an ssh_config fragment, written to
  `~/.config/sshelf/ssh_config`; add one `Include` line to `~/.ssh/config` yourself (sshelf
  still never edits it, or anything under `~/.ssh`). Plain `ssh`/`scp`/`sftp`, rsync, git, and
  anything that reads SSH config — VS Code Remote-SSH, JetBrains Gateway — then resolve sshelf
  hosts by name, with users, ports, identities, jump hosts, and inherited site defaults
  included. Exact `-o Key=Value` extra-args translate to real directives; other raw flags stay
  visible as a comment. Once the file exists it refreshes automatically on every hosts change
  (add/edit/delete, import, site changes); output is deterministic and diff-friendly.
  `--stdout` prints the fragment instead of writing. No new dependencies.

### Changed
- Docs: [the site](https://max-rh.github.io/sshelf/) now opens with a full user guide —
  install, quickstart, per-feature pages, CLI reference, configuration, FAQ — and the README
  is a shorter landing page that links into it. No behavior changes.

## [0.9.0] — 2026-06-23

### Added
- **Install from [crates.io](https://crates.io/crates/sshelf)** — `cargo install sshelf` (published
  automatically on each release).
- **RedHat/Fedora `.rpm` packages** (x86_64 + aarch64) attached to every release, built as a static
  musl binary so they run on any RPM distro (Fedora, RHEL/Rocky/Alma, openSUSE) regardless of glibc.

## [0.8.0] — 2026-06-23

### Added
- **Two-factor (2FA) hosts** — flag a host (add/edit form, or `sshelf add … --2fa`) whose login
  needs an interactive verification code (TOTP / keyboard-interactive). On connect, sshelf shows a
  popup to enter the current code and supplies it to the prompt through the same `SSH_ASKPASS`
  helper that supplies a stored password — fixing connects that previously failed, because a
  stored-secret connect runs with `SSH_ASKPASS_REQUIRE=force` (which routes the code prompt to the
  helper with no terminal fallback). `sshelf <host>` from the CLI prompts on the terminal. Manual
  entry only — no TOTP seeds are stored. No new dependencies.

## [0.7.0] — 2026-06-22

### Added
- **Background port forwarding** (`Ctrl-f` on a host): a popup to start a **Local** (`-L`),
  **Remote** (`-R`) or **Dynamic** (`-D` SOCKS) SSH tunnel, reusing the host's auth exactly as
  connect does (keys / agent / ProxyJump / stored password, plus site defaults). The forward runs
  as a detached background process that **keeps running after you quit sshelf**, and a bind/auth
  failure (port in use, privileged port, server refused) is reported in the popup. A new
  **forwards manager** (`F4`) lists every active forward across hosts with its pid and age and
  stops any (`d`/`k` → `y`); the list refreshes live and is reconciled against the running
  processes on each launch, so it only ever shows forwards that are still actually running.
  Tracked in `forwards.json`. No new dependencies.

## [0.6.0] — 2026-06-20

### Added
- **Sites** — group hosts (one site per host, e.g. a data center / project), distinct from the
  many-valued free-form tags. A site can carry **optional** shared SSH defaults — user, port,
  ProxyJump (bastion), identity — that member hosts inherit at connect time where the host
  leaves a field unset (the host always wins; auth stays per-host). The list **groups by site**
  when idle and shows a `·site·` column + `site:NAME` filter while typing. Manage sites with
  **F3** (create/edit/delete + their defaults); assign one in the add/edit form. CLI: `sshelf
  sites [--json]`, `sshelf sites add NAME [-u/-p/-J/-i]`, and `sshelf add --site NAME`. Stored in
  `hosts.toml` as `[[site]]` — old files load unchanged.

## [0.5.0] — 2026-06-16

### Added
- **Dual-pane file transfer** (`Ctrl-t` on a host): a two-pane browser — local files on one
  side, the host's on the other — to copy files and folders either direction over SFTP, with
  fuzzy search on both sides and live progress. Authenticates **once** via an `ssh` ControlMaster
  that reuses the host's existing auth (keys / agent / ProxyJump, or a stored password through
  `SSH_ASKPASS`), then runs `sftp` over it — no PTY, no per-file re-prompt, and `~/.ssh/config`
  is never touched. `Tab` switches panes, `→`/`Enter` opens a directory, `Ctrl-s` sends the
  selection, `Esc` cancels; a same-named destination is skipped (not overwritten) and symlinks
  are flagged and skipped. No new dependencies.
- **`--transfer-log <FILE>`** (also `$SSHELF_TRANSFER_LOG`): append the transfer screen's
  `ssh`/`sftp` commands and their errors to a file for debugging — no secrets are logged.

## [0.4.0] — 2026-06-14

### Added
- **`sshelf add`** opens the TUI add form when run bare, and adds a host **non-interactively**
  when given arguments (`NAME` + `--hostname` required; `--user/--port/--auth/--identity/--jump/
  --tag/--extra`, and `--password-stdin` to store a secret without it touching argv). Auth is
  inferred from `--identity`/`--password-stdin`. Replaces the previous placeholder message.
- **`sshelf list --json`** — machine-readable output (each host's fields plus its generated
  `ssh` command), always valid JSON; a stable surface for scripts and integrations.
- **`sshelf -`** — reconnect to the most-recently-used host.
- **Dynamic shell completion** of saved host names (`clap_complete` engine). Enable with
  `source <(COMPLETE=<shell> sshelf)`; completes the `<host>` of direct-connect, `print-command`,
  and `set-password`.
- CI: a dependency-audit job (`cargo audit`) and an MSRV (1.88) check.

### Changed
- README states the no-network posture explicitly: no telemetry, no account, no network calls
  of sshelf's own.
- `SECURITY.md` now lists concrete private-reporting channels (GitHub security advisories +
  email) and documents the vault-mode environment tradeoff.

### Fixed
- The vault master passphrase (`SSHELF_VAULT_PASSPHRASE`) is no longer inherited by the
  exec'd `ssh` for hosts with no stored secret. (For hosts that use a stored secret in vault
  mode it remains available to the askpass helper, which requires it — now documented.)

## [0.3.0] — 2026-06-12

### Added
- **Print command:** `sshelf print-command <host>` prints the generated, shell-quoted `ssh …`
  command for a saved host without connecting or updating frecency — the CLI equivalent of the
  TUI's `Ctrl-y` yank. (#3)

### Fixed
- Generated/yanked command strings now expand identity-file `~` before shell-quoting, so a
  quoted path (e.g. one containing spaces) stays copy-paste runnable. (#3)

## [0.2.0] — 2026-06-07

### Added
- **Direct connect:** `sshelf <host>` connects to a saved host by name or id without opening
  the TUI — same connect path as the TUI (frecency recorded, stored secret auto-supplied).
  A miss suggests the closest matching names.
- **List filtering:** `sshelf list [query]` filters with the same syntax as the TUI search
  box — fuzzy text and/or `tag:NAME` tokens (e.g. `sshelf list tag:prod`).

## [0.1.0] — 2026-06-06

Initial public release.

- Fuzzy-search TUI launcher for saved SSH hosts (type to filter, `Enter` to connect),
  atuin-style, with tag filters (`tag:NAME`) and frecency ordering.
- Connect hands the terminal to `ssh` via `exec()` — on logout you're back at your shell.
- Add/edit/delete via a single-screen, auth-aware form; `.pem`-aware key picker with an
  in-TUI file browser; quick-add with sensible defaults.
- Password / key-passphrase auto-supply via `SSH_ASKPASS`: secrets live in the OS keyring
  (macOS Keychain / Linux Secret Service) or an `age`-encrypted vault for headless use —
  never in `hosts.toml`, never on the command line.
- Jump-host chains (`-J`), custom ports, extra ssh flags per host.
- Read-only import from `~/.ssh/config` (`sshelf import`, `Ctrl-o`).
- `Ctrl-y` yanks the generated `ssh` command; `F2` settings (hosts-file location);
  `sshelf completions <shell>` and `sshelf man`.
- Packaging: Homebrew tap, shell installer, Debian/Ubuntu `.deb` (x86_64 + arm64, macOS +
  Linux).

[Unreleased]: https://github.com/max-rh/sshelf/compare/v0.13.0...HEAD
[0.13.0]: https://github.com/max-rh/sshelf/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/max-rh/sshelf/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/max-rh/sshelf/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/max-rh/sshelf/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/max-rh/sshelf/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/max-rh/sshelf/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/max-rh/sshelf/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/max-rh/sshelf/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/max-rh/sshelf/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/max-rh/sshelf/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/max-rh/sshelf/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/max-rh/sshelf/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/max-rh/sshelf/releases/tag/v0.1.0
