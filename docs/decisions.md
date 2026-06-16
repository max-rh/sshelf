# Decision log

ADR-style. Newest on top. Each entry: the decision, why, and what we rejected. Add an entry
whenever you make a non-trivial design choice.

---

### D-019 · File transfer rides an `ssh` ControlMaster; `sftp`/`scp` as subprocesses
The dual-pane transfer screen moves files over the **system `sftp`/`scp` binaries**, not a Rust
SSH library: every pure-Rust option either pulls C deps (libssh2) or forces `tokio` and can't
reuse sshelf's `SSH_ASKPASS`/ProxyJump auth. To support password hosts without a fragile PTY,
sshelf authenticates **once** by opening a backgrounded `ssh` **ControlMaster** (reusing
`build_args` + the askpass env exactly as connect does); `sftp`/`scp` then ride it with only
`-o ControlPath`, so there is no re-auth and no per-file prompt. A spike against a local sshd
confirmed that (a) `SSH_ASKPASS` supplies the secret to open the master and (b) `sftp`/`scp`
ride it for put/get and recursive copies. The ride commands deliberately omit `-p`/`-i`/`-J`
(the master already carries them) — which also avoids the `ssh -p` vs `sftp`/`scp -P` port-flag
clash. Rejected: `ssh2`/`wezterm-ssh` (C deps), `russh`/`openssh-sftp-client` (tokio + no askpass
reuse), and a PTY password screen-scraper (brittle, locale/version-dependent).

**Update (transfers use `sftp`, not `scp`):** listing and copying both run through `sftp`
(`ls`/`get`/`put`). `scp` was dropped after a filename with spaces failed in testing — OpenSSH 9+
`scp` speaks the SFTP protocol and takes the remote path *literally*, so shell-quoting it (needed
by legacy `scp`) injects literal quotes. `sftp` quotes via its own command parser consistently
across OpenSSH versions, so one quoting rule (`shell_quote`) is correct everywhere.

### D-018 · Configurable hosts file in config; config file via flag/env only
A `hosts_file` key in `config.toml` relocates the host DB (editable via the F2 settings screen,
default under the config dir). The **config file's own** location can't be a config key
(bootstrap/circular), so it's set with `--config` / `$SSHELF_CONFIG` only and shown read-only in
settings. The `--config` flag is plumbed by setting `$SSHELF_CONFIG` once at startup so every
`Paths::resolve()` (incl. subcommands) sees it uniformly. Vault/state stay in the XDG data dir,
so askpass is unaffected by a custom config. On hosts-file change, an existing target is adopted
(never overwritten) and config is committed only after the hosts step succeeds (so a bad path
can't brick startup). Designed to grow (more settings fields later).

### D-017 · Pick keys via a file browser; detect keys by header
The Key field cycles `~/.ssh` keys with `←/→` and opens an in-TUI **file browser** on `Enter`
so users can pick a key **anywhere** (e.g. an AWS `.pem` in `~/Downloads`) without typing a
path. Key discovery detects private keys by a `PRIVATE KEY` header (not just a `.pub` sibling),
so `.pem`/keyless keys are found. Chosen over a path text field (the user explicitly didn't
want to paste paths) and over scanning many fixed locations (a browser is more general).

### D-016 · Auth-aware wizard with a single-key picker
The add/edit form shows only the fields relevant to the chosen auth method, and `key` auth uses
a picker over `~/.ssh` keys (files with a `.pub` sibling) rather than a freeform path field.
Matches the user's request and reduces clutter. Trade-off: the picker selects one key; a host
with multiple identity files keeps them on edit, but adding several is done via `hosts.toml`
(the model still supports `Vec`). Discovery uses `OsString` (no lossy UTF-8) so keys aren't missed.

### D-015 · askpass answers password + passphrase, matched by prompt shape
The helper now supplies the host's stored secret for **both** login-password and key-passphrase
prompts (a host uses one auth method, so one secret suffices), enabling auto-supply for
encrypted keys. To prevent a keyboard-interactive server from phishing the secret, matching is
by OpenSSH prompt **shape** (ends-with `password:` / contains `passphrase for`), not a bare
substring. Connect wires `SSH_ASKPASS` only when a stored secret exists (`wire_askpass`).

### D-014 · age vault uses scrypt (passphrase recipient), not Argon2id
The earlier plan said Argon2id; `age`'s passphrase mode actually uses **scrypt** + ChaCha20-Poly1305.
We use `age`'s built-in passphrase encryptor rather than composing a KDF/AEAD by hand (avoids
nonce-reuse/parameter footguns). Docs corrected to say scrypt.

### D-013 · Secret backend chosen by `SSHELF_VAULT_PASSPHRASE` (v1)
OS keyring by default; if `SSHELF_VAULT_PASSPHRASE` is set, use the age vault instead. Chosen
over runtime keyring-availability detection + an interactive passphrase modal because it's
deterministic, scriptable (headless/CI), and avoids a TUI passphrase prompt plus askpass-side
unlock in v1. Trade-off: headless users set the env var (shell profile / systemd). Auto-detect
fallback + interactive prompt are future enhancements. A `set-password` CLI provisions secrets
without the TUI.

### D-012 · Project name: `sshelf`
Chosen over `ssh-tui` (generic), `sssh` (one keystroke from `ssh`, typo-prone), `hopp` (low
discoverability). `sshelf` = "a shelf for your SSH hosts": brandable, memorable, still
contains "ssh" for search discoverability. Confirmed available on crates.io.

### D-011 · Docs-in-sync rule
Every code/behavior change updates `docs/` + `docs/progress.md` in the same change; the rule
lives in `CONTRIBUTING.md`. Rationale: keep a publishable, never-stale knowledge base for an
open-source project and its contributors.

### D-010 · License: dual `MIT OR Apache-2.0`
Rust ecosystem norm (ratatui, ripgrep, crossterm). Maximizes downstream compatibility vs.
single MIT or AGPL. AGPL rejected (limits commercial adoption for a CLI tool).

### D-009 · Platforms: macOS + Linux only (v1)
`exec()` process replacement is Unix-only and the secret backends differ on Windows. Windows
would need a separate spawn+wait path + Credential Manager — deferred to a later version.

### D-008 · Frecency = `use_count * exp(-decay_rate * days_since_last_used)`
Mozilla Places–style. Simple, explainable, self-adjusting. Idle list sorts by frecency;
while typing, fuzzy score dominates and frecency breaks ties. `decay_rate` (default 0.2) is
configurable. Rejected: pure recency (ignores frequency), pure alphabetical (ignores usage).

### D-007 · Read-only import via `ssh2-config`
Best-maintained Rust SSH-config parser. It intentionally skips `Match`/`Include`, so import
must warn and degrade, not silently drop. We never write back to `~/.ssh/config`.

### D-006 · Config/data paths: `etcetera` base strategy (XDG everywhere)
`~/.config/sshelf` on **both** macOS and Linux (honoring XDG env vars). Rejected `directories`
crate's native strategy, which buries macOS files in `~/Library/Application Support` — worse
for a hand-editable CLI tool. State/vault go in the XDG data dir.

### D-005 · Host DB format: TOML (`hosts.toml`), not SQLite
Human-readable and hand-editable — matches the "my own transparent store" intent; host counts
are small (tens–hundreds). Atomic writes (temp+rename) prevent corruption. One research stream
suggested SQLite for indexed frecency queries; rejected for v1 as overkill, but it's a clean
future migration if scale demands.

### D-004 · Frecency state separate from `hosts.toml` (`state.json`)
Mutable counters churn on every connect; keeping them out of the user-owned host file keeps
that file stable and diff-friendly. Keyed by stable host `id` so renames preserve history.

### D-003 · Two-tier secrets: OS keyring primary + `age` vault fallback
keyring (Keychain / Secret Service) for desktops; an `age`-encrypted vault (master passphrase,
in-memory per session) for headless/minimal Linux with no Secret Service daemon — exactly the
boxes this tool targets. `age` (used by atuin) chosen over hand-rolled Argon2+ChaCha to avoid
error-prone crypto. Secrets are **never** stored in `hosts.toml`.

### D-002 · Password auto-supply: `SSH_ASKPASS` (+ `REQUIRE=force`), not `sshpass`
Our own binary is the askpass helper (detected via `SSHELF_ASKPASS=1`; ssh calls it as
`sshelf "<prompt>"`). No external dependency; secret never appears in `ps`/argv. Mandatory
consequence: the helper must inspect `argv[1]` and only answer password prompts, and we set
`-o StrictHostKeyChecking=accept-new` to keep host-key prompts away from it. Validated by the
M0 spike before anything builds on it. Rejected `sshpass`: not installed by default, exposes
the password in the process table.

### D-001 · Connect = tear down TUI then `exec()` into `ssh` (exit-to-shell)
User chose exit-to-shell over return-to-list. `exec()` (process replacement) gives ssh the
real TTY cleanly. Consequence: nothing runs after `exec()`, so frecency is persisted *before*
the handoff. Rejected spawn+wait (would be needed only for return-to-list).

### D-000 · Stack: Rust + ratatui + crossterm, sync event loop, component pattern
Matches atuin's look/feel (user preference). ratatui 0.30 requires **Rust 1.88+**
(`rustup update` mandatory). Synchronous `crossterm::event::read()` loop — no tokio, since the
only long-running task (the SSH session) happens after the TUI exits. Component-per-screen
structure over the Elm pattern for this app's modal UI.
