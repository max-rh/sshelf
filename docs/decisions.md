# Decision log

ADR-style. Newest on top. Each entry: the decision, why, and what we rejected. Add an entry
whenever you make a non-trivial design choice.

---

### D-022 · Interactive 2FA: collect the code before connect, inject it via the askpass helper
A connect that auto-supplies a stored secret runs `ssh` with `SSH_ASKPASS_REQUIRE=force`, which
routes **every** interactive prompt — including a server's keyboard-interactive verification-code
step — to our askpass helper; the helper declined it, and a spike confirmed `force` gives **no
terminal fallback**, so the code prompt was answered empty and auth failed (a real user hit this).
A during-session popup is impossible (connect `exec()`s into `ssh`), and a PTY screen-scraper was
already rejected (D-019). So 2FA is handled the same way the password is: a per-host
**`requires_2fa`** flag makes connect show a small code popup *before* the `exec()` (while the TUI
is alive); the entered one-time code is passed to `ssh` via `SSHELF_2FA_CODE` (like the vault
passphrase already rides env), and the helper answers the **non-secret** prompt with it. The
helper's routing: a password/passphrase-shaped prompt → the stored secret (unchanged, anti-phish
guard intact); any other prompt → the queued code; else decline. `configure_askpass` therefore
force-wires the helper when a secret exists **or** a code is queued (so key+2FA hosts work too).
The CLI direct-connect path (`sshelf <host>` / `-`), which has no TUI, prompts for the code on the
terminal before handoff. Rejected: storing the TOTP **seed** and generating the code ourselves
(puts the second factor in the same vault as the first, and needs a TOTP dep the project avoids);
auto-detecting 2FA with no flag (we can't probe the server's auth methods without a separate
non-`exec` connection). Note: a host with **no** stored secret already prompts for the code inline
after handoff (no askpass forced), so the flag/popup mainly fixes the stored-secret case; an
encrypted key with no stored passphrase + 2FA should use an agent (else `force` askpass can't
answer the passphrase prompt either). v1 is manual entry only.

### D-021 · Port forwards are detached `ssh -N` processes tracked by PID
Background port forwards (`Ctrl-f` popup, `F4` manager) must keep running after sshelf exits.
Each forward is **one detached `ssh -N -L|-R|-D <spec>` process**, reusing `ssh::build_args` +
`ssh::configure_askpass` (so keys/agent/ProxyJump/stored-password and site defaults all work as
connect does). It is spawned with `std::os::unix::process::CommandExt::process_group(0)` (std,
**no new dep**) and null stdin/stdout, which makes it survive both sshelf exiting (orphaned →
reparented to init) and the terminal closing (its own process group never receives the shell's
SIGHUP). **Nothing kills a forward on `Drop` or app shutdown** — that is what keeps it alive.
Validated by an M0 spike: a `process_group(0)` child with null stdio outlives its spawner
(PPID→1) in its own process group, and `kill -TERM` stops it.

There is no daemon. The running processes are the source of truth; `forwards.json` (mirrors
`state.json`: `#[serde(transparent)]` over a `Vec`, `atomic_write` `0600`) is just a remembered
list of PIDs. `reconcile` re-validates each PID via `ps -ww -o state=,command=`: a forward stays
only if the process exists, isn't a zombie (`state != Z` — so a dead-but-unreaped child we
spawned this session is correctly seen as gone), **and** its command line still matches our
`ssh … <spec>` (a **PID-reuse guard** — a recycled pid is never counted alive or signalled).
Reconcile runs on startup, on opening the manager, and on the ~100ms event-loop tick while it's
open. Readiness/errors: `-o ExitOnForwardFailure=yes` makes ssh exit non-zero on a bind failure;
spawn polls `try_wait` for ~2.5s and, on an early exit, maps the stderr (captured to a temp file,
not a pipe, so a long-lived ssh never gets SIGPIPE) to a friendly message (port in use,
privileged port, server refused, auth failed). A third kind, **Dynamic** (`-D` SOCKS), was added
alongside Local/Remote. Rejected: a worker thread per forward (the transfer model — unneeded, a
forward has no ongoing protocol to service, just liveness); holding the `Child` for `try_wait`
(can't track forwards from a previous session, and splits liveness into two code paths); `ssh -f`
(clean daemonize but hides the real PID, breaking the reuse guard and individual kill);
`libc::setsid`/`nix::kill` (a new dep the project avoids — `process_group(0)` + shelling to
`ps`/`kill`, as we already shell to `ssh`/`sftp`, is dep-free); kill-only for v1 (restart of a
dropped forward is deferred — the spec is persisted, so it's an easy fast-follow).

### D-020 · Sites: one-per-host grouping with optional inherited SSH defaults
Hosts can belong to a **Site** (a data center / project), distinct from many-valued free-form
`tags`. A site is **one per host** and may carry **optional** shared SSH defaults — `user`,
`port`, `jump_hosts` (the bastion), `identity_files` — that members inherit at connect time
**only where the host leaves that field unset** (the host always wins). A bare site (name only)
is pure grouping. **Auth is not inheritable** (it stays per-host; inheriting it would change
which fields apply and surprise users — a site can still carry a default identity that only
takes effect for key-auth members). Inheritance is computed by resolving a host into an
"effective host" (`Host::with_site_defaults`) at every Host→ssh-args boundary (connect, yank,
transfer master, CLI print/list-json), leaving `ssh::build_args` untouched — chosen over
threading `&[Site]` through `build_args` and its many callers/tests. Hosts reference a site **by
name**; an undefined name **degrades gracefully** (pure grouping, no inheritance, no error).
Stored in `hosts.toml` as `[[site]]` (sites before hosts; `format_version` unchanged — old files
load with `sites = []`). The list **groups by site when idle** and shows a flat `·site·` column
+ `site:NAME` filter while typing. Renames in the F3 manager **cascade** to member hosts;
deleting a site **clears** members' `site` (self-healing) rather than leaving a dangling name.
Rejected: a single special tag (too weak — no inherited config); a separate sites file (one
atomic `hosts.toml` is simpler and keeps the reference local).

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
