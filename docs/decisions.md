# Decision log

ADR-style. Newest on top. Each entry: the decision, why, and what we rejected. Add an entry
whenever you make a non-trivial design choice.

---

### D-028 · Transfer panes list hidden files on both sides, with no toggle
The remote pane listed through `sftp`'s own `ls -l`, which drops dot-entries, while the local
pane read the directory itself and kept them. A host's `.config` was therefore invisible, with
no way to walk into it (#15). The listing now uses `ls -la` and the two sides agree.

Rejected: a show/hide-hidden toggle. It would need a key (and `Ctrl-h` is Backspace in most
terminals), a line in the help overlay, a docs section, and a decision about whether the local
pane follows the same switch. The narrowing case is already covered by the filter that is
there: type a `.` and the pane keeps the names that have one. A real toggle can be its own
small change if people ask for it.

`.` and `..` are still dropped in `parse_ls_line`. That check was written defensively before
those entries could arrive; with `-a` they really do. Sorting is untouched (directories first,
then case-insensitive by name), so dotfiles land wherever that comparator puts them.

### D-027 · `doctor` is local, read-only, and exit-code shaped, with no `--json` yet
`sshelf doctor` exists because every support question so far has had one of six causes, and
none of them is visible from inside sshelf: an OpenSSH older than 8.4, a secret backend that
isn't there, a hand-edited `hosts.toml`, a `site` that was renamed away, an agent that isn't
running, an export fragment that drifted. One command that names all six, and the fix for each,
is worth more than six FAQ entries, so the FAQ now points at it instead.

**Local only.** Every check reads the filesystem, the environment, the secret backend, or
`ssh -V`. No pings, no test connections, no version lookups. sshelf's no-network posture (D-024,
`security.md`) is a promise, not a default, so it isn't relaxed for diagnostics: `doctor`
reports whether *sshelf* is set up, never whether a *host* is up. Rejected: a connectivity probe
per host, which would be the most-requested feature of the command and the first thing to turn it
into a monitoring tool, which is explicitly out of scope.

**Read-only, with one stated exception.** The keyring check writes a throwaway entry
(`sshelf-doctor-probe`, under the real `sshelf` service so it exercises the actual path) and
deletes it immediately, because a backend that reads but can't write is a backend that fails the
first time a password is saved, and a read-only probe would pass. Everything else, including
the vault check, only reads.

**Exit 0 unless something failed; warnings don't fail the run.** That split is what makes
`sshelf doctor && ...` usable in a script: `fail` means broken now, `warn` means it works but is
limited (a stale export, no agent for agent hosts, orphaned secrets). A per-check severity with
no exit-code contract would leave every caller inventing its own parser.

**Orphan detection is vault-only, and says so.** The `keyring` crate offers no portable way to
enumerate a service's entries (macOS would need `security dump-keychain`, which prompts), so in
keyring mode the check reports that it did not run. Rejected: silently reporting `ok`, because a
clean bill of health nobody checked is worse than an honest gap.

**Export staleness is compared by content, not mtime.** The fragment renders deterministically
(D-023), so identical content *is* an up-to-date file whatever the timestamps say, and a
touched-but-unchanged file shouldn't nag.

**No `--json` in this release.** The exit code already covers the scriptable case, and a
machine-readable report has no user yet; adding one now would freeze a schema before anything
consumes it. Demand-gated, like the rest.

### D-026 · Transfer multi-select: positional marks, a serial queue, and `mkdir` (not `mkdir -p`)
`Space` marks the entry under the cursor and `Ctrl-a` marks everything the filter shows (pressing
it again clears every mark); `Ctrl-s` then sends the marked set, the loudest missing piece next
to a real file manager. Marks are **positional** (indices into the pane's current listing) and are
dropped whenever that listing is replaced: a directory change, a refresh after a transfer, or a
listing error. Rejected: remembering marks per path across navigation, which needs a second
model of the remote tree and raises the question of what a mark means after the file behind it
changed. Positional marks are always either right or gone. `Space` therefore stops being a filter
character. Filenames with spaces still match by the rest of their name, and marking beats
a literal space at position one of a query.

A send becomes a **queue**, not parallel transfers: the worker owns one ControlMaster and runs one
`sftp` at a time, and one authenticated connection is the whole point of that design (D-019).
The queue is captured at send time, so later navigation can't change what moves, and
sending **consumes the marks**, so the queue is now the record. A destination that already holds the
name is **skipped and the queue continues** (never-overwrite still holds, and one collision
shouldn't cost the other nine); a genuine transfer *failure* stops the rest, because a broken link
or a full disk will break the next item too. The destination is refreshed once, when the queue
drains, rather than after every item, because a listing is a round trip.

Cancelling mid-queue now emits a `Cancelled` event. Before, the worker cancelled silently and
the screen, which blocks every other key while a transfer runs, stayed stuck in that state.

`F7` (mc's key, with `Ctrl-f` as an alias for terminals that keep the function keys) opens a
one-line input on the focused pane. It creates **one** directory in that pane's current
directory: `std::fs::create_dir` locally, `sftp`'s own `mkdir` remotely, deliberately **not**
`create_dir_all` / `mkdir -p`. A name carrying a path is a mistake, not a shortcut, and silently
creating three intermediate directories from a typo is exactly the kind of surprise this tool
avoids. Names with `/`, control characters, `.`/`..`, or an existing entry are refused with the
input still open; an existing directory is **never adopted**, matching how transfers refuse to
overwrite. On success the listing refreshes and the new directory lands under the cursor.

### D-025 · tmux integration: window/pane modes, stay in the picker, and why secrets never cross
One config key (`tmux = "off" | "window" | "pane"`, default `"off"`) plus a `$TMUX` check.
When both say yes, `Enter` spawns `tmux new-window`/`split-window` and sshelf **keeps running**;
that is the feature, not a side effect. The picker's cost per connection is what makes people
stop reaching for it, and a tmux user wants four sessions, not four launches. Outside tmux, or
with the key off, the path is bit-for-bit the old one: tear down, `exec()`, exit to shell (D-001).
Frecency is persisted before the spawn for the same reason it is persisted before `exec()`: the
connection is gone from sshelf's hands either way.

The ssh argv is handed to tmux as **separate arguments after `--`**, so tmux `execvp`s it
directly and no shell re-splits an identity path containing a space.

The hard part is authentication. The askpass wiring normally rides on the child `Command`'s
environment, which a tmux window (a child of the tmux *server*, not of sshelf) never inherits.
tmux's only way across is `new-window -e KEY=VALUE`, and **those are the tmux client's own
argv**: world-readable via `ps`, which is precisely the leak D-002 exists to prevent. So the line
is drawn by content, not convenience:

- Key/agent hosts: a plain spawn, no `-e` at all.
- Stored-secret hosts: `-e` carries `SSH_ASKPASS`, `SSH_ASKPASS_REQUIRE=force`,
  `SSHELF_ASKPASS=1` and `SSHELF_HOST_ID`. None is a secret: the id is an opaque ULID the helper
  trades for the real secret out of the keyring, exactly as it does after an `exec()`.
- **A queued 2FA code** (`SSHELF_2FA_CODE`) and **the vault master passphrase**
  (`SSHELF_VAULT_PASSPHRASE`) never cross. Those connections **fall back to `exec()` in place**,
  with a one-line reason printed after the TUI is down and before ssh starts.
- **tmux older than 3.0** has no `-e`, so stored-secret hosts fall back there too; an unreadable
  or unparseable `tmux -V` counts as too old, since falling back is always correct.

Rejected: writing the code or passphrase to a temp file for the new window to read (a second
secret-at-rest path, with cleanup that a killed pane never runs); `tmux setenv` before spawning
(same argv exposure, plus it leaks into the whole session); and refusing tmux mode for every
secret-bearing host (it would exclude ordinary keyring password hosts, which are safe, for the
sake of two that aren't). A unit test asserts the code and passphrase variables can never appear
in a generated tmux argv.

### D-024 · Tailscale import shells out to the user's CLI; MagicDNS names, suffix eligibility, add-only
`sshelf import --tailscale` fills the database from a tailnet, the inventory a homelab/small-fleet
user already curates, without sshelf growing a network client. It **shells out to the user's own
`tailscale status --json`** and parses stdout. Rejected: the Tailscale HTTP API / an SDK, which
would mean an API key to store (violating the no-secrets-in-`hosts.toml` rule and the whole
secret-handling story), an HTTP + async dependency stack, and sshelf making network calls of its
own; and reading `tailscaled`'s local socket directly (unstable, root-ish, platform-specific).
Shelling out keeps the **no-network posture intact**: the binary runs only when the user runs the
subcommand, never at startup, on save, on a timer, or from the TUI. Binary resolution is
`$SSHELF_TAILSCALE_BIN` → `PATH` → `/Applications/Tailscale.app/Contents/MacOS/Tailscale`,
because the macOS app doesn't put its CLI on `PATH`.

**`hostname` = the MagicDNS FQDN**, not a Tailscale IP: it survives IP churn, reads better in the
list, and is what Tailscale SSH expects, with the IP (IPv4 first) as the fallback when MagicDNS
is disabled for the tailnet. **Eligibility is one rule: the peer's `DNSName` must sit under the
tailnet's own `MagicDNSSuffix`**, which excludes Mullvad exit nodes and shared-in/foreign nodes
without special-casing either. Rejected: filtering by `ExitNodeOption`/`OS`/owner (a list of
special cases that ages badly). Expired peers are skipped; **offline peers are not**, because
`Online` is transient and an asleep laptop is still a real host. The tailnet becomes a **site**
(matched case-insensitively, created bare when new) and ACL tags become sshelf tags minus the `tag:`
prefix, so the tailnet's own structure carries over instead of being flattened.

Import is **add-only**: existing hosts and sites are never updated or deleted, so re-running
converges to "0 added" and the user's own edits (a `user`, a port, a password) are never
clobbered. Rejected: sync semantics (two-way reconciliation, deletion of departed nodes), which
needs a per-host record of provenance, and v1 deliberately stores **nothing** tailscale-specific
in `hosts.toml`: no node IDs, no keys. The JSON→hosts mapping is a pure function over `&str`
(mirroring `import::parse_str`), so the whole feature is fixture-tested without the binary,
a tailnet, or a network.

### D-023 · Export writes our own ssh_config fragment; the user adds the Include line
`sshelf export` projects the database into ssh_config format so native tools (ssh/scp/sftp,
rsync, git, editor remote extensions) resolve sshelf hosts by name. The standing objection to
a tool with its own database is lock-in, and this removes it. The fragment is written to
**sshelf's config dir** (`ssh_config`, next to `hosts.toml`), never under `~/.ssh`; the user
adds the one `Include` line themselves, which keeps the "never edits `~/.ssh/config`" promise
literal (sshelf still only ever *reads* it, to skip the hint when the Include is already there).
Rendering mirrors `ssh::build_args` (site defaults resolved, `-i` gated on key auth, port 22
omitted) **minus sshelf-only plumbing**: no `StrictHostKeyChecking=accept-new` (that exists to
keep the host-key prompt away from the askpass helper; plain ssh should keep the user's
defaults) and no askpass wiring (exported password hosts prompt on the tty). `extra_args` are
CLI flags, not config keywords, so only exact `-o Key=Value` pairs translate; the rest is
preserved as an in-block comment rather than guessed at. Output is deterministic (name-sorted,
no timestamps) so the file churns only when the database changes. **Once the file exists,
every hosts save refreshes it** (best-effort, since derived data never blocks a save); creating it
is the opt-in, deleting it opts out. Host names that can't be a safe `Host` pattern (glob/
negation/comment/quote characters) are skipped with a comment, since exporting them would match
*other* names; values are control-char-stripped so a crafted field can't inject directives.
Rejected: writing into `~/.ssh/` (even a new file, since the promise is that sshelf never touches
that directory); auto-appending the Include (mutates the user's config, the one hard no);
translating arbitrary flags to directives (lossy guessing); a config key for the export path
(existence-as-opt-in needs one well-known location).

### D-022 · Interactive 2FA: collect the code before connect, inject it via the askpass helper
A connect that auto-supplies a stored secret runs `ssh` with `SSH_ASKPASS_REQUIRE=force`, which
routes **every** interactive prompt, including a server's keyboard-interactive verification-code
step, to the askpass helper; the helper declined it, and a spike confirmed `force` gives **no
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
auto-detecting 2FA with no flag (sshelf can't probe the server's auth methods without a separate
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
SIGHUP). **Nothing kills a forward on `Drop` or app shutdown**, and that is what keeps it alive.
Validated by an M0 spike: a `process_group(0)` child with null stdio outlives its spawner
(PPID→1) in its own process group, and `kill -TERM` stops it.

There is no daemon. The running processes are the source of truth; `forwards.json` (mirrors
`state.json`: `#[serde(transparent)]` over a `Vec`, `atomic_write` `0600`) is just a remembered
list of PIDs. `reconcile` re-validates each PID via `ps -ww -o state=,command=`: a forward stays
only if the process exists, isn't a zombie (`state != Z`, so a dead-but-unreaped child sshelf
spawned this session is correctly seen as gone), **and** its command line still matches our
`ssh ... <spec>` (a **PID-reuse guard**, so a recycled pid is never counted alive or signalled).
Reconcile runs on startup, on opening the manager, and on the ~100ms event-loop tick while it's
open. Readiness/errors: `-o ExitOnForwardFailure=yes` makes ssh exit non-zero on a bind failure;
spawn polls `try_wait` for ~2.5s and, on an early exit, maps the stderr (captured to a temp file,
not a pipe, so a long-lived ssh never gets SIGPIPE) to a friendly message (port in use,
privileged port, server refused, auth failed). A third kind, **Dynamic** (`-D` SOCKS), was added
alongside Local/Remote. Rejected: a worker thread per forward (the transfer model, unneeded here
because a forward has no ongoing protocol to service, just liveness); holding the `Child` for
`try_wait` (can't track forwards from a previous session, and splits liveness into two code paths);
`ssh -f` (clean daemonize but hides the real PID, breaking the reuse guard and individual kill);
`libc::setsid`/`nix::kill` (a new dep the project avoids, since `process_group(0)` + shelling to
`ps`/`kill`, as sshelf already shells to `ssh`/`sftp`, is dep-free); kill-only for v1 (restart of a
dropped forward is deferred, but the spec is persisted, so it's an easy fast-follow).

### D-020 · Sites: one-per-host grouping with optional inherited SSH defaults
Hosts can belong to a **Site** (a data center / project), distinct from many-valued free-form
`tags`. A site is **one per host** and may carry **optional** shared SSH defaults (`user`,
`port`, `jump_hosts` (the bastion), `identity_files`) that members inherit at connect time
**only where the host leaves that field unset** (the host always wins). A bare site (name only)
is pure grouping. **Auth is not inheritable** (it stays per-host; inheriting it would change
which fields apply and surprise users, though a site can still carry a default identity that only
takes effect for key-auth members). Inheritance is computed by resolving a host into an
"effective host" (`Host::with_site_defaults`) at every Host→ssh-args boundary (connect, yank,
transfer master, CLI print/list-json), leaving `ssh::build_args` untouched, chosen over
threading `&[Site]` through `build_args` and its many callers/tests. Hosts reference a site **by
name**; an undefined name **degrades gracefully** (pure grouping, no inheritance, no error).
Stored in `hosts.toml` as `[[site]]` (sites before hosts; `format_version` unchanged, so old files
load with `sites = []`). The list **groups by site when idle** and shows a flat `·site·` column
+ `site:NAME` filter while typing. Renames in the F3 manager **cascade** to member hosts;
deleting a site **clears** members' `site` (self-healing) rather than leaving a dangling name.
Rejected: a single special tag (too weak, with no inherited config); a separate sites file (one
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
(the master already carries them), which also avoids the `ssh -p` vs `sftp`/`scp -P` port-flag
clash. Rejected: `ssh2`/`wezterm-ssh` (C deps), `russh`/`openssh-sftp-client` (tokio + no askpass
reuse), and a PTY password screen-scraper (brittle, locale/version-dependent).

**Update (transfers use `sftp`, not `scp`):** listing and copying both run through `sftp`
(`ls`/`get`/`put`). `scp` was dropped after a filename with spaces failed in testing. OpenSSH 9+
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
path. Key discovery detects private keys by a `PRIVATE KEY` header rather than only a `.pub`
sibling, so `.pem`/keyless keys are found. Chosen over a path text field (the user explicitly didn't
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
deterministic, scriptable (headless/CI), and avoids a TUI passphrase prompt plus an askpass-side
decrypt in v1. Trade-off: headless users set the env var (shell profile / systemd). Auto-detect
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
would need a separate spawn+wait path + Credential Manager, so it is deferred to a later version.

### D-008 · Frecency = `use_count * exp(-decay_rate * days_since_last_used)`
Mozilla Places style. Simple, explainable, self-adjusting. Idle list sorts by frecency;
while typing, fuzzy score dominates and frecency breaks ties. `decay_rate` (default 0.2) is
configurable. Rejected: pure recency (ignores frequency), pure alphabetical (ignores usage).

### D-007 · Read-only import via `ssh2-config`
Best-maintained Rust SSH-config parser. It intentionally skips `Match`/`Include`, so import
must warn and degrade, not silently drop. We never write back to `~/.ssh/config`.

### D-006 · Config/data paths: `etcetera` base strategy (XDG everywhere)
`~/.config/sshelf` on **both** macOS and Linux (honoring XDG env vars). Rejected `directories`
crate's native strategy, which buries macOS files in `~/Library/Application Support`, worse
for a hand-editable CLI tool. State/vault go in the XDG data dir.

### D-005 · Host DB format: TOML (`hosts.toml`), not SQLite
Human-readable and hand-editable, which matches the "my own transparent store" intent; host
counts are small (tens to hundreds). Atomic writes (temp+rename) prevent corruption. One research
stream suggested SQLite for indexed frecency queries; rejected for v1 as overkill, but it's a clean
future migration if scale demands.

### D-004 · Frecency state separate from `hosts.toml` (`state.json`)
Mutable counters churn on every connect; keeping them out of the user-owned host file keeps
that file stable and diff-friendly. Keyed by stable host `id` so renames preserve history.

### D-003 · Two-tier secrets: OS keyring primary + `age` vault fallback
keyring (Keychain / Secret Service) for desktops; an `age`-encrypted vault (master passphrase,
in-memory per session) for headless/minimal Linux with no Secret Service daemon, exactly the
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
(`rustup update` mandatory). Synchronous `crossterm::event::read()` loop, with no tokio, since the
only long-running task (the SSH session) happens after the TUI exits. Component-per-screen
structure over the Elm pattern for this app's modal UI.
