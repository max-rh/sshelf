# Progress log

Reverse-chronological. Newest entry on top. Every change to the project adds an entry here
(the docs-in-sync rule). Keep entries short: what changed, why, and what's next.

**Current milestone:** Sites — group hosts with optional inherited SSH defaults, targeting
**v0.6.0**. v1 acceptance gates closed.

---

## 2026-06-17 — Sites: grouped/flat host list (M2)

- The host list now **groups by site** when idle (`── {site} ({n}) ──` section headers, sites
  alphabetical, `(no site)` last) and shows a flat ranked list with a dim `·site·` column while
  filtering. `recompute` builds a grouped `order` when the query is empty (`group_order`);
  `order` still holds host indices only, so selection/navigation are unchanged — the renderer
  maps the selected host past the non-selectable headers to the `ListState` index.
- Tests: `group_order` sectioning (case-insensitive, `(no site)` last); render checks for the
  grouped headers + the filtered site column. 135 tests; clippy + fmt clean.
- Next: the wizard site chooser + F3 sites manager (M3), then CLI (M4).

---

## 2026-06-17 — Sites: model + inheritance + search (M1)

- New **Site** concept: a one-per-host grouping that may carry **optional** shared SSH defaults
  (user/port/jump/identity) member hosts inherit at connect time. Bare site = pure grouping;
  per-host fields always override; auth stays per-host. Distinct from many-valued `tags`.
- `model.rs`: `Site` struct + `Host.site: Option<String>` (by name) + `HostsFile.sites`
  (`[[site]]`, sites-first; no `format_version` bump — old files load unchanged). Inheritance
  via `Host::with_site_defaults(&[Site])` (clone, fill only unset fields, id preserved; unknown
  site name degrades to plain grouping) + `find_site` (case-insensitive). `search_haystack`
  includes the site.
- `search.rs`: `parse_query` now also yields an optional `site:NAME` token; `rank` filters by it.
- Threaded resolution into every Host→ssh-args boundary: TUI connect/yank/transfer, CLI
  connect/`-`/print-command/`list --json` command. `App.sites` loaded + persisted (and it
  follows an F2 hosts-file move). Verified end-to-end via `print-command` + `list --json`.
- 132 tests (model inheritance/degradation, `site:` filter, store round-trip + pre-sites
  back-compat); clippy + fmt clean. No UI yet.
- Next: the grouped/flat list (M2), then the wizard chooser + F3 sites manager (M3), CLI (M4).

---

## 2026-06-16 — Transfer: `--transfer-log` diagnostics

- Added a transfer debug log: `sshelf --transfer-log <FILE>` (or `$SSHELF_TRANSFER_LOG`) appends
  every `ssh`/`sftp` command the worker runs plus its full stderr to `FILE`, so a failed transfer
  can be inspected after the fact (the status line still shows the one-line cause). No secrets are
  logged — the password reaches `ssh` via `SSH_ASKPASS`, never argv. The e2e test asserts the log
  captures the master + `get`/`put` commands. Docs: README, `ux.md` (CLI table + transfer
  section), `security.md`.

---

## 2026-06-16 — Transfer: use `sftp` (not `scp`) for the copy itself

- Bug found in local testing: transferring a filename with **spaces** failed
  (`scp: failed to upload … to '/…`). OpenSSH 9+ `scp` speaks the SFTP protocol and takes the
  remote path literally, so the shell-quoting legacy `scp` needed became *literal quotes* in the
  name. Plain names slipped through because they aren't quoted.
- Fixed by running transfers through **`sftp` `get`/`put`** over the same master used for
  listing — `sftp` quotes via its own command parser consistently across OpenSSH versions, so
  the version-dependent `scp` quoting trap is gone. Removed `scp_args`/`remote_spec`; added a
  `transfer_batch` unit test and a spaces regression to the e2e test.

---

## 2026-06-16 — Transfer screen: transport core + worker

- Started the dual-pane SFTP/SCP **transfer screen**. Settled the transport (see `decisions.md`
  D-019): move files over the system `sftp`/`scp` riding a single `ssh` **ControlMaster**, so
  keys/agent/ProxyJump and the stored keyring/vault secret are reused unchanged and password
  hosts work with no PTY. A spike against a local sshd confirmed `SSH_ASKPASS` opens the master
  and that `sftp`/`scp` ride it (put/get + recursive).
- Landed the tested core in `src/transfer/mod.rs`: the master/`sftp`/`scp` argv builders, the
  `user@host` target + shell-quoted remote-path spec, the worker↔UI message protocol, and
  progress math.
- Added the worker thread + ControlMaster lifecycle (`src/transfer/worker.rs`): it opens the
  master (reusing `ssh::configure_askpass`, now `pub(crate)`), polls it ready, lists remote
  dirs by parsing `sftp ls -l`, runs `scp` transfers with throttled progress + mid-flight
  cancel, and tears the master + control socket down on stop via RAII. 101 tests; clippy + fmt
  clean. No UI yet; the live end-to-end run lands with the engine milestone.
- Added `transfer/pane.rs`: one side's state — fuzzy filter + selection + navigation reused
  from the key-picker browser, a synthetic `..` entry, `ls -F`-style dir/`@`-symlink labels with
  control-char stripping, and a local-directory reader. Kept source-agnostic rather than behind
  a `DirSource` trait (a synchronous remote `list()` would block the very UI loop the worker
  keeps responsive); the screen feeds local entries via `std::fs` and remote ones via the worker.
  109 tests; clippy + fmt clean.
- Wired the screen end to end: `transfer/screen.rs` (two panes over one session — local nav is
  synchronous, remote nav requests via the worker, events drained each tick) and `ui/transfer.rs`
  (two panes, progress/status line, hint bar; `TestBackend`-snapshotted via a borrowed view, and
  a "terminal too small" clamp). `Ctrl-t` on the list opens it (`Outcome::Transfer`); the event
  loop polls + drains while it's open and tears the worker down on close (RAII). Keys: `Tab`
  switch · `→`/`Enter` open · `Ctrl-s` send file/folder · `←`/`Backspace` up · `Esc` cancel/
  clear/close. Docs: `ux.md` transfer section + keybinding. 113 tests; clippy + fmt clean.
- Validated the transport end to end against a throwaway localhost `sshd` (`transfer/e2e.rs`,
  `#[ignore]`d — run with `cargo test -- --ignored`): the master opens, `sftp` `pwd`/`ls`
  parse, single-file download + upload (contents verified), and recursive directory download
  all pass.
- Robustness + docs pass: a same-named destination is **skipped** rather than clobbered;
  README gains a feature bullet + the `^t` key, `security.md` covers the transfer network path,
  and `structure.md` maps the new modules. Added master-command tests for ProxyJump + password
  hosts — the auth itself reuses `build_args`/`configure_askpass` (already tested), and the M0
  spike proved `SSH_ASKPASS` opens the master, so a password *target* and key/agent jumps work;
  a fully automated password-auth transfer E2E needs a PAM/Docker sshd (the rootless test server
  is key-auth only) and is a CI-with-Docker follow-up.
- The transfer screen is **functionally complete**: dual-pane browse + fuzzy on both sides,
  single-file and recursive folder transfer in both directions over one multiplexed master,
  live progress, cancel, and overwrite-skip. 116 unit tests + 1 e2e; clippy + fmt clean.

---

## 2026-06-13 — CLI: non-interactive add, list --json, dynamic completion, reconnect-last

- **`sshelf add` gained flags** for a fully non-interactive add (scripts/dotfiles): `NAME` +
  `-H/--hostname` required; `-u/-p/-a/-i/-J/-t/--extra/--password-stdin`. Auth is inferred
  (`key` from `--identity`, `password` from `--password-stdin`, else `agent`). `--extra` allows
  hyphen-leading values; `--password-stdin` keeps the secret out of argv. Bare `sshelf add`
  still opens the TUI form. Duplicate names are refused. (`AddArgs::into_host` is pure/tested.)
- **`sshelf list --json`** emits each selected host's fields plus its generated `command`,
  always valid JSON (even empty) — the stable surface for integrations.
- **Dynamic shell completion** of host names via `clap_complete` (`unstable-dynamic`):
  `CompleteEnv` in `main`, `ArgValueCandidates` on the `<host>` args of direct-connect /
  `print-command` / `set-password`; `host_name_candidates` reads `hosts.toml` side-effect-free.
  Enable with `source <(COMPLETE=<shell> sshelf)`.
- **`sshelf -`** reconnects to the most-recently-used host (`last_used_id` over the frecency
  state); the CLI connect path was factored into a shared `connect()`.
- 99 tests; clippy + fmt clean; verified end-to-end (add/list --json/password-stdin/completion).
- Docs: README (usage + an "Adding hosts from the CLI" flag table + a "Shell completions"
  section) and the `docs/ux.md` CLI table.

---

## 2026-06-12 — CLI: print generated ssh command

- Added `sshelf print-command <host>`: prints the same shell-quoted `ssh …` command as the
  TUI `Ctrl-y` yank action, without connecting or updating frecency. Useful for scripts,
  wrappers, and review before running a connection.
- Fixed generated command strings to expand identity-file `~` before shell-quoting, so yanked
  or printed commands remain copy-paste runnable.
- Docs synced: README usage, `docs/ux.md` CLI table, and `docs/ssh-command.md` builder note.

---

## 2026-06-07 — Pre-launch hardening

- **`sshelf add` now opens the TUI with the add form ready** (`app::run_add`) — it was a
  placeholder message. Empty-list hint and internal comments cleaned of milestone references.
- **Vault env hygiene:** `configure_askpass` strips `SSHELF_VAULT_PASSPHRASE` from the child
  env when no stored secret is wired; kept (and now documented) for vault-mode askpass, which
  reads it as ssh's child. Two new env-wiring tests (`ssh.rs`).
- **SECURITY.md:** concrete private-reporting channels (GitHub advisories + email) replace the
  placeholder note; added the vault-mode env-inheritance tradeoff (mirrored in
  `docs/security.md` + `docs/ssh-command.md`).
- **CHANGELOG.md** added (backfilled 0.1.0 / 0.2.0); README now states the no-network posture
  (no telemetry / account / network calls) and documents `sshelf add`.
- **CI:** new `cargo audit` (RustSec) and MSRV-1.88 check jobs.

---

## 2026-06-07 — Release v0.2.0

- Cut **v0.2.0**: ships the `sshelf <host>` direct-connect and `sshelf list <query>` filter
  (below). Tagging `v0.2.0` republishes brew / shell installer / `.deb` via dist.

---

## 2026-06-07 — CLI: direct connect + list filter

- `sshelf <host>` connects straight to a saved host by name/id, skipping the TUI (reuses the TUI
  connect path: frecency recorded before `exec`, askpass wired only when a secret exists). A miss
  suggests close names. Clap routes via `args_conflicts_with_subcommands`, so subcommand names win.
- `sshelf list [query]` filters with the same syntax as the TUI search box (`search::rank`): fuzzy
  text and/or `tag:NAME`. Plain `sshelf list` is unchanged.
- 88 tests (added clap-routing + host-resolution); clippy + fmt clean. Docs: README usage + a brew
  completion-reload note; new `docs/ux.md` CLI section.

---

## 2026-06-07 — README demo GIF

- Added an animated demo to the top of the README (`docs/sshelf-readme.gif`): fuzzy-search →
  yank the generated `ssh` command.

---

## 2026-06-06 — v0.1.0 released

- First public release is live: dist's `Release` workflow built all four targets, created the
  GitHub Release (tarballs + shell installer), and published the Homebrew formula; `release-deb`
  attached the amd64/arm64 `.deb`s. All jobs green.
- README **Install** section rewritten for the real channels (Homebrew, shell installer, `.deb`,
  from source). `docs/packaging.md` synced to the shipped setup: `dist-workspace.toml` config,
  `workflow_run` sequencing of the `.deb` job, and the `HOMEBREW_TAP_TOKEN` prerequisite.

---

## 2026-06-06 — Release pipeline: dist (cargo-dist) wired up

- `dist init`: shell + Homebrew installers, 4 Unix targets (mac + linux × x86_64/arm64),
  `install-updater = false`. Added `release.yml`, `dist-workspace.toml`, and `[profile.dist]`.
- Dropped the `x86_64-pc-windows-msvc` target dist added by default — sshelf is Unix-only
  (the connect path uses `exec()`), so a Windows build can't compile.
- Reworked `release-deb.yml` to run via `workflow_run` after the dist `Release` workflow
  finishes, attaching the `.deb`s to the release dist creates — avoids both workflows racing
  to create the same release.
- Before tagging: create the `max-rh/homebrew-tap` repo + a `HOMEBREW_TAP_TOKEN` secret (PAT)
  so the Homebrew formula can be published.

---

## 2026-06-06 — CI: fix the push trigger

- `ci.yml` listened on `main`, but the default branch is `master`, so direct pushes never ran
  CI. Now triggers on `[master, main]`.

---

## 2026-06-06 — Funding notes: trim public meta-commentary

- Removed the BTC-address caveat from the README Support section (the donate badge + address stay).
- Trimmed the `.github/FUNDING.yml` comment down to the functional config.

---

## 2026-06-06 — Docs: contributor guide + naming polish

- Adopted **`CONTRIBUTING.md`** as the contributor guide (GitHub-conventional name) and
  refreshed its cross-references in `docs/{index,structure,decisions}.md`.
- Standardized the **"docs-in-sync rule"** naming across the docs.
- No code changes.

---

## 2026-06-05 — Post-v1: browser fuzzy search, dynamic wizard width, settings screen ✅

- **File browser fuzzy search** — type to filter the listing (nucleo); `Backspace` edits the
  filter (else up-dir), `Esc` clears it (else cancels). Shared `ui::highlight` between the host
  list and browser.
- **Dynamic wizard width** — the add/edit form sizes to the terminal (clamped 56–100), fixing
  placeholder truncation; longest placeholders trimmed; placeholders now read `optional ·` /
  `required ·`.
- **Settings screen (`F2`)** + `ui/settings.rs`: edit the **hosts-file** location (default shown;
  `~` expanded), config-file path shown read-only. New `hosts_file` config key; `--config` flag +
  `$SSHELF_CONFIG` env (plumbed via env so subcommands + askpass-irrelevant paths stay uniform);
  `Config::save`/`hosts_path`; `App.hosts_path` threaded through list/import/set-password.
- **Fix:** the hosts-file relocate could overwrite an existing target with
  the (possibly empty) in-memory hosts → now it **adopts** an existing file and only writes through
  to a new path, committing config only on success. Two app-level tests cover both branches.
- Help overlay height bumped (the F2 line was clipping). 84 tests; clippy + fmt clean.
- **Deviation to confirm:** "custom config file" is via `--config`/env (shown read-only in
  settings), not editable in the wizard — the bootstrap-correct interpretation.
- Snapshots: `target/{wizard,browse,settings}-snapshot.txt`.

---

## 2026-06-05 — Post-v1: .pem keys + in-TUI file browser ✅

Follow-up to the wizard work (user requests):
- **`.pem` / keyless keys are detected** — `scan_keys` includes any private key by sniffing a
  `PRIVATE KEY` header, not just `<name>.pub` pairs (AWS keys show up).
- **File browser** (`ui/browse.rs`) — the Key field opens it with `Enter` (`←/→` still cycles
  recent `~/.ssh` keys); navigate dirs and pick a key **anywhere** without typing a path.
  A browsed path is stored as the host's identity even outside `~/.ssh`.
- **Placeholders** now mark fields `optional ·` / `required ·`. The Key field's hint becomes
  "←/→ recent keys · ↵ browse files" when focused.
- 75 tests (incl. `scan_keys` against a temp dir with a `.pem`, browser nav, Enter→browse);
  clippy + fmt clean. Snapshots: `target/{wizard,browse}-snapshot.txt`.
- **Acceptance gate:** the browser + Enter→browse→pick flow is `TestBackend`-only; a real-TTY
  run (open the Key field, browse to a `.pem`, pick, save, connect) is still pending — folded
  into gate #2 below.

---

## 2026-06-05 — Post-v1: auth-aware wizard, key picker, key-passphrase auto-supply ✅

User-requested wizard improvements:
- Every field shows a dim **placeholder** explaining it.
- The form is **auth-aware** — only relevant fields show: key → Key picker + optional Key
  passphrase; password → Password; agent → neither.
- **Key picker** cycles private keys discovered under `~/.ssh` (files with a `.pub` sibling).
- **Key passphrase** (optional) is stored as the host secret; askpass now answers passphrase
  prompts too, and connect wires askpass whenever a stored secret exists (password OR passphrase).

Hardening review — confirmed and fixed:
- the "password NOT stored" message → "secret NOT stored" (applies to key passphrases too);
- `is_secret_prompt` tightened to OpenSSH prompt *shapes* (ends-with `password:` / contains
  `passphrase for`) so a keyboard-interactive server can't phish the stored secret;
- `discover_ssh_keys` no longer uses lossy UTF-8 conversion (won't miss/corrupt keys);
- editing a multi-key host no longer drops the extra identity files.
- Dismissed false alarms: env-clearing already unconditional, the keyring check is fail-closed,
  multi-key-passphrase is out of scope. Skipped 2 lows (wide-char mask cosmetics; the already
  documented macOS double-Keychain-prompt on unsigned builds).
- 66 tests; clippy `-D warnings` + `cargo fmt --check` clean.

---

## 2026-06-05 — M8: OSS readiness ✅

- **Linux verified for real** (Docker `rust:latest`): build + all 63 tests pass. The first
  Linux build *caught a bug* — `sync-secret-service` pulled the C `libdbus-sys` (needs
  `libdbus-1-dev`). Switched to **pure-Rust** `async-secret-service` + `crypto-rust` +
  `async-io` → no C/OpenSSL/tokio build deps. (Closes acceptance gate #3.)
- `README.md`, `SECURITY.md` (threat model + macOS-signing note), `LICENSE-MIT` +
  `LICENSE-APACHE` (dual), and `.github/workflows/ci.yml` (fmt + clippy + build + test on
  macOS & Linux, plus a **headless-vault job** that stores/retrieves via the age vault with
  `DBUS_SESSION_BUS_ADDRESS` unset — verified locally).
- `cargo fmt` applied repo-wide so the CI format check passes.
- 63 tests; clippy `-D warnings` clean on macOS and Linux.

## 2026-06-05 — M7: read-only import from ~/.ssh/config ✅

- `import.rs`: `ssh2-config 0.7.1` parse (`ALLOW_UNKNOWN_FIELDS`) → `Host` mapping (name,
  hostname, user, port, identity files; the parser expands `~` to an absolute path). Skips
  wildcard patterns; **warns** about `Match` / `Include` / `ProxyJump` (unsupported).
- `Ctrl-o` in the TUI imports all *new* (non-duplicate-by-name) hosts; `sshelf import [--dry-run]`
  does the same from the CLI. Never writes `~/.ssh/config`.
- **Verified against the real `~/.ssh/config`**: parsed 4 hosts read-only (mtime unchanged),
  correct mapping, `--dry-run` wrote nothing.
- v1 deviation: no in-flight per-host *selection* UI — it imports all new hosts, then you
  curate with edit/delete (recorded in `docs/ux.md`).
- 63 tests pass; clippy `-D warnings` clean.

---

## 2026-06-06 — Distribution: dist + .deb + clap completions/man (chosen stack)

Picked the channels (GitHub user `max-rh`): **dist/cargo-dist** for Homebrew + tarballs +
shell installer, **cargo-deb** for Debian/Ubuntu, **clap** for completions/man, **no crates.io**.
- Code: added `sshelf completions <shell>` and `sshelf man` subcommands (`clap_complete` /
  `clap_mangen` via `Cli::command()` — no build.rs). Verified bash/zsh/fish + roff output.
- Packaging: `[package.metadata.deb]` in `Cargo.toml` (depends `openssh-client`, recommends
  `gnome-keyring`, ships completions + man); `.github/workflows/release-deb.yml` builds amd64
  (`ubuntu-22.04`) + arm64 (`ubuntu-24.04-arm`) natively and attaches `.deb`s to the `v*` Release
  (upserts alongside dist's `release.yml`).
- `docs/packaging.md` rewritten around this stack (multi-arch x86+arm, dist `init` choices,
  the deb companion, the macOS signing/Keychain note, manual Homebrew formula + APT repo in an
  appendix). dist's `release.yml` itself is generated by `dist init` (documented).
- §6 reframed: **no paid Apple Developer Program needed** — a CLI via Homebrew runs unsigned
  (Homebrew doesn't quarantine formulae; arm64 just needs the free auto **ad-hoc** signature).
  Paid Developer ID/notarization is optional (only removes Gatekeeper friction for *direct*
  `.tar.gz` downloads). Vault stays the free Keychain fallback.
- Chose **option 3 (free ad-hoc signing)**: verified on this Intel Mac that a default build is
  "not signed at all" and `codesign --sign - --force` → `Signature=adhoc`; documented the exact
  step + where it slots into dist's `release.yml` (§6). No paid Apple program.
- Email: advised an alias (public in `.deb`/repo); `authors` made optional. License: keep dual
  MIT OR Apache-2.0. Funding: **BTC only for now** (GitHub Sponsors needs a payout setup) — README
  **Support** section + `.github/FUNDING.yml` (custom→README).
- Pre-public-push scan: clean (no real keys/personal email/host IPs). Swapped a coincidental LAN
  IP in a test for the RFC5737 doc range; set `Cargo.toml` repository/homepage to max-rh/sshelf.
- 84 tests; clippy + fmt clean. BTC address filled in. Ready for the initial public push
  (branch `master`).

---

## ⚠ Unverified paths (acceptance gates before "done")

These are verified by unit tests but NOT yet exercised on a real path; treat as manual
acceptance gates (a sandbox can't cover them):

1. **macOS OS-keyring path** — only the **vault** secret path (`SSHELF_VAULT_PASSPHRASE`) is
   verified end-to-end. The default macOS path (no env var → Keychain) is unrun; an unsigned
   dev build's re-exec'd askpass child may hit a Keychain access prompt per connect (ACLs are
   keyed to code signature). Run from a real macOS GUI session; until then, the **vault is the
   recommended setup** and is what's been proven.
2. **The full in-TUI connect chain has never run as one piece.** For a *password* host it is:
   real TTY → `exec_connect` (which sets `SSH_ASKPASS`/`SSHELF_*` env) → `exec(ssh)` → ssh
   re-execs `sshelf` (askpass mode) as a child, which resolves paths + fetches the secret. The
   M5 E2E hand-set the env and called `ssh` directly — it did **not** go through `exec_connect`;
   and `TestBackend` doesn't touch raw mode / alt-screen. **Acceptance test: connect to a real
   password host *from inside the TUI*** (not just `ssh`), and exercise the **key file browser**
   (open the Key field → `Enter` → browse to a `.pem`, type-to-filter → pick → save → connect) and
   the **F2 settings** relocate (change the hosts file, confirm it adopts/relocates correctly). If
   macOS Keychain prompts on every connect for the unsigned dev build, that's expected → use the
   vault or a signed build.
3. ~~**Linux build**~~ — ✅ **closed (M8)**: built + tested in Docker `rust:latest` (63 tests
   pass) with the pure-Rust `async-secret-service` backend; CI now builds/tests Linux + a
   headless `DBUS_SESSION_BUS_ADDRESS`-unset vault job. (First real CI run still pending.)

---

## 2026-06-05 — M6: tags, config, theme, frecency wiring ✅

- **Tag filtering** (the explicitly-chosen v1 feature): `tag:NAME` tokens in the query AND
  every tag (case-insensitive, exact); remaining words fuzzy-match. Combine freely
  (`tag:prod web`). Help overlay + hint bar updated.
- **`default_sort` wired into the TUI** (was list-CLI only): empty query honors
  frecency-or-name from config.
- **`config.toml` made real:** a commented default is written on first run (TUI or `list`),
  with `decay_rate`, `default_sort`, and a new `accent` color (themes the UI via a one-time
  color cell). Default-template parse is tested.
- **Deleted dead `error.rs`** (committed fully to `anyhow`).
- 59 tests pass; clippy `-D warnings` clean. Verified default config write + tag filter.

---

## 2026-06-05 — M5: secrets + password auto-supply ✅ (verified end-to-end)

- `vault.rs`: age-encrypted (`age 0.10.1`, scrypt + ChaCha20-Poly1305) `host_id → password`
  map; store/get/delete + atomic writes. `secrets.rs`: routes to the **OS keyring** by default,
  or the **vault** when `SSHELF_VAULT_PASSPHRASE` is set (deterministic, headless/CI-friendly).
  `keyring 3.6.3` with per-target backends (apple-native / sync-secret-service / windows-native).
- `askpass.rs`: headless `SSH_ASKPASS` mode — inspects `argv[1]`, answers only password prompts
  (fetches by `SSHELF_HOST_ID`), declines everything else with exit 1.
- `ssh.rs`: `configure_askpass` sets `SSH_ASKPASS`/`REQUIRE=force`/`SSHELF_ASKPASS`/`SSHELF_HOST_ID`
  for password hosts only, clearing inherited askpass otherwise.
- Wizard gained a masked **Password** field; save stores the secret; delete removes it.
- New `sshelf set-password <name|id>` CLI (reads stdin) for headless/scripted provisioning.
- **End-to-end verified** with the real binary against the live password sshd: `set-password`
  → vault; askpass returns the secret for a password prompt and **declines** a host-key prompt
  (exit 1); and a full `ssh` (SSH_ASKPASS=sshelf) logged in with **no prompt** (`PW_AUTOSUPPLY_OK`).
- 54 tests pass (vault round-trip, prompt classification, wizard password capture); clippy clean.

---

## 2026-06-05 — M4: add / edit / delete ✅

- `ui/widgets.rs`: hand-rolled single-line `TextField` (insert/backspace/cursor moves).
- `ui/wizard.rs`: full-screen add/edit **form** (9 focusable fields: name, hostname, user,
  port, auth toggle, identity, jump hosts, tags, extra args) with inline validation; returns
  `WizardOutcome {Continue, Cancel, Save(Host)}`. Chose a single-screen form over a paged
  wizard (simpler/editable); `ux.md` updated.
- `app.rs`: `Ctrl-a` add, `Ctrl-e` edit (prefilled), `Ctrl-d` delete (confirm popup). Save
  upserts by id and writes `hosts.toml` atomically; delete also drops the frecency entry.
- Verified add-persists-to-disk and delete via tests (incl. reload-from-disk). Wizard render
  snapshot at `target/wizard-snapshot.txt`.
- 46 tests pass; clippy `-D warnings` clean.

---

## 2026-06-05 — M3: connect via exec() + yank ✅

- `ssh.rs`: `build_args` (`-i` per key with `~` expansion, `-p` only if non-22, `-J` comma
  chain, `-o StrictHostKeyChecking=accept-new`, shlex-split extra args, `user@host`);
  `command_string` (readable, tilde-preserved, for yank); `exec_connect` via
  `CommandExt::exec` (unix process replacement); `copy_to_clipboard` (arboard, best-effort).
- `app.rs`: `Enter` → `Outcome::Connect`, `Ctrl-y` → `Outcome::Yank`. Connect defers to *after*
  `ratatui::restore()`: `run` records frecency + saves state, then `exec`s ssh (clean TTY).
  Panic-safety is handled by ratatui's `init()` panic hook (no separate RAII guard needed).
- Added `shlex 2.0.1`, `arboard 3.x` (no-default-features, text-only).
- **Verified:** recreated the spike sshd with a public key and connected with the exact
  `build_args` flag set (`-i … -p 2222 -o StrictHostKeyChecking=accept-new tester@127.0.0.1`)
  → `CONNECT_OK`. (Interactive TUI→exec is TTY-only; argv logic is unit-tested and the live
  connection is proven here.)
- 33 tests pass; clippy `-D warnings` clean (collapsed nested ifs into 1.88 let-chains).

---

## 2026-06-05 — M2: core TUI (list + fuzzy search) ✅

Added `ratatui 0.30.0` + `nucleo-matcher 0.3.1`. The atuin-style launcher renders: search box
(with `matched/total` in the title), highlighted fuzzy list, contextual hint bar, F1 help overlay.

- `search.rs`: nucleo fuzzy ranking; empty query → frecency order, else score desc with
  frecency tiebreak; `match_indices` for per-char highlight.
- `app.rs`: `App` + pure `on_key` returning `Outcome {Continue, Quit, Connect(idx)}`, plus the
  sync event loop using `ratatui::init()/restore()`. Single-mode search → **Ctrl-based actions**
  (resolved the plain-letter-vs-typing conflict; `ux.md` updated).
- `ui/{mod,list,help}.rs`: rendering as pure fns of `&App`, verified with `TestBackend`
  (no TTY). ASCII snapshot written to `target/tui-snapshot.txt`.
- 25 tests pass; clippy `-D warnings` clean. Connect currently shows a placeholder status;
  the real `exec()` handoff is M3.

---

## 2026-06-05 — M1: scaffold + persistence ✅

Crate `sshelf` (edition 2024, `rust-version = 1.88`, license `MIT OR Apache-2.0`) builds clean
with `clippy -D warnings`; 12 unit tests pass.

- Deps resolved: `serde 1.0.228`, `toml 1.1.2`, `serde_json 1.0.150`, `etcetera 0.11.0`,
  `clap 4.6.1`, `thiserror 2.0.18`, `anyhow 1.0.102`, `ulid 1.2.1`.
- Modules: `model` (Host/AuthMethod/HostsFile + ULID ids), `paths` (XDG via `etcetera::Xdg`
  → `~/.config/sshelf` confirmed on macOS), `store` (TOML load/save + atomic temp+rename),
  `state` (frecency: `use_count`/`last_used`, `score = count·e^(−decay·days)`), `config`
  (decay_rate, default_sort), `error` (typed `SshelfError`).
- `main`: clap CLI (`list`/`add`/`import`), askpass-via-env dispatch stub, `list` works and
  sorts by frecency. Verified end-to-end against `examples/hosts.sample.toml`.
- Forward-declared API (`save_hosts`, `atomic_write`, `state::save/record_use`, `Host::new`,
  `find`, `search_haystack`) carries `#[allow(dead_code)]` + a milestone note; each allow is
  removed as the function is wired up.
- **Note:** cargo defaulted to **edition 2024**; updated the project guide accordingly.

---

## 2026-06-05 — M0: askpass mechanism validated (spike) ✅

Empirically validated the password auto-supply design against a real password-auth sshd
(Docker `lscr.io/linuxserver/openssh-server`, OpenSSH 10.2 client) on macOS. Also bumped the
toolchain: **Rust 1.74 → 1.96.0** via `rustup update` (clears the ratatui 0.30 MSRV gate).

- **Test 1 (success):** `SSH_ASKPASS=helper SSH_ASKPASS_REQUIRE=force` + `PreferredAuthentications=password`
  + `StrictHostKeyChecking=accept-new` → **logged in, exit 0**. Confirms `SSH_ASKPASS` satisfies
  interactive `PasswordAuthentication` (not just key passphrases). The helper received
  `argv[1] = "tester@127.0.0.1's password: "`.
- **Test 2 (host-key routing):** with `StrictHostKeyChecking=ask` + fresh known_hosts, ssh sent the
  helper the host-key prompt (`"…Are you sure you want to continue connecting (yes/no/[fingerprint])?"`),
  and a naive "always return the password" helper caused an **infinite loop** on
  `"Please type 'yes', 'no' or the fingerprint:"`.
- **Conclusions (both already in the design):** the helper **must inspect `argv[1]`** and answer only
  password prompts (exit non-zero otherwise), and we **must pass `-o StrictHostKeyChecking=accept-new`**
  so the host-key prompt never reaches it. See [ssh-command.md](./ssh-command.md) §3.
- Spike container kept running (`sshelf-spike`, host port 2222) for reuse in M5.

---

## 2026-06-05 — Documentation foundation

- Created the project guide (the docs-in-sync rule + the hard project invariants).
- Created the `docs/` tree: `index`, `progress`, `architecture`, `structure`, `data-model`,
  `ssh-command`, `ux`, `decisions`, `security` — all seeded from the project plan.
- No Rust code yet. Toolchain still on Rust 1.74 — **must `rustup update` to 1.88+** before M1.
- **Next:** M0 askpass spike (validate password auto-supply on macOS + Linux before building on it).

---

## Milestones

Tracking against the project plan. Status: ⬜ not started · 🟡 in progress · ✅ done.

| # | Milestone | Status |
|---|---|---|
| — | Docs foundation (project guide + `docs/`) | ✅ |
| M0 | Spike `SSH_ASKPASS` password mechanism | ✅ (macOS; Linux pending in CI) |
| M1 | Scaffold crate + persistence (`paths`/`model`/`store`, clap, licenses) | ✅ |
| M2 | Core TUI: list + fuzzy search + highlight + hint bar | ✅ |
| M3 | Connect via `exec()` handoff (key/agent hosts) + yank | ✅ |
| M4 | Add/Edit/Delete wizard (+ quick-add) | ✅ |
| M5 | Secrets (keyring + age vault) + password auto-supply (askpass) | ✅ |
| M6 | Polish: frecency tuning, tags, config, help, theme | ✅ |
| M7 | Read-only import from `~/.ssh/config` | ✅ |
| M8 | OSS readiness: README, SECURITY, CI, licenses | ✅ |

The full milestone detail lives in the project plan.
