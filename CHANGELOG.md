# Changelog

Notable, user-facing changes per release. Follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions follow SemVer.

## [Unreleased]

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

[Unreleased]: https://github.com/max-rh/sshelf/compare/v0.9.0...HEAD
[0.9.0]: https://github.com/max-rh/sshelf/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/max-rh/sshelf/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/max-rh/sshelf/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/max-rh/sshelf/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/max-rh/sshelf/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/max-rh/sshelf/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/max-rh/sshelf/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/max-rh/sshelf/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/max-rh/sshelf/releases/tag/v0.1.0
