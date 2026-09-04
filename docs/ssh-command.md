# SSH command generation & the askpass mechanism

This is the heart of `sshelf` and its trickiest part. Read carefully before touching `ssh.rs`
or `askpass.rs`.

## 1. Building the `ssh` argv

From a `Host`, build (in order):

```
ssh
  [-i <identity_file>]...            # one -i per entry in identity_files (auth = "key")
  [-p <port>]                        # only if port present and != 22
  [-J <jump1,jump2,...>]             # ProxyJump chain (jump_hosts), comma-joined
  -o StrictHostKeyChecking=accept-new   # see §3: keeps host-key prompt away from askpass
  <extra_args...>                    # raw, split with `shlex`, appended verbatim
  <user>@<hostname>                  # user defaults to $USER if unset
```

- Pure flags only, with **no temporary `ssh -F` config files** (keeps the "never touch SSH
  config" promise literal and avoids cleanup).
- `extra_args` is the escape hatch for anything the wizard doesn't model (`-X`, `-L ...`,
  `-o ...`). Split with `shlex::split` so quoted args survive.
- Example: stored host `mike@10.25.25.25` with key `~/.ssh/infra-key` →
  `ssh -i /home/mike/.ssh/infra-key -o StrictHostKeyChecking=accept-new mike@10.25.25.25`
  in the printed/yanked command (the exec path expands `~` internally as well).

The same builder backs the `Ctrl-y` **yank** action and `sshelf print-command <host>`
(copy/print the exact command without connecting). For copy/paste safety, identity-file `~`
is expanded before shell-quoting; quoted `~` would not expand in the user's shell.

## 2. Launch handoff (`exec`)

On connect:

1. **Persist frecency first** (`exec()` never returns, so nothing runs after it).
2. Set environment for the child:
   - `SSH_ASKPASS = <path to sshelf's own binary>` (`std::env::current_exe()`)
   - `SSH_ASKPASS_REQUIRE = force`
   - `SSHELF_ASKPASS = 1`        ← how the re-exec'd binary knows it's in askpass mode
   - `SSHELF_HOST_ID = <id>`     ← which secret to fetch
   - `env_remove("SSH_ASKPASS")` of any *inherited* value first, then set ours (avoid pollution).
3. Tear down the TUI: `disable_raw_mode()` → `LeaveAlternateScreen` → show cursor → flush.
4. `std::os::unix::process::CommandExt::exec()` into `ssh`. If it returns, it errored →
   restore terminal, show the error.

A RAII guard + panic hook guarantees step 3's teardown also runs on panic/early-exit.

### 2a. The tmux handoff

With `tmux = "window"`/`"pane"` **and** `$TMUX` present, steps 3 and 4 are replaced by a spawn and
sshelf keeps running:

```
tmux new-window|split-window
  [-e SSH_ASKPASS=<self>] [-e SSH_ASKPASS_REQUIRE=force]
  [-e SSHELF_ASKPASS=1]   [-e SSHELF_HOST_ID=<id>]      # only when a secret is stored
  [-n <host name>]                                       # new-window only; split-window has no -n
  --                                                     # ends tmux's own options
  ssh <the argv from §1, as separate arguments>
```

- Frecency is still persisted **first**; the spawn is as much a point of no return as `exec()`.
- The argv is passed as separate arguments, never one joined string, so tmux `execvp`s it and a
  path containing a space survives.
- `-e` pairs land in the tmux client's argv, so only non-secret wiring may ride there. A queued
  2FA code, a vault master passphrase, or a tmux older than 3.0 (no `-e`) sends the connection
  back to the `exec()` path above, with the reason printed once the TUI is down. See
  [`security.md`](./security.md) and D-025.

## 3. Secret auto-supply and the sharp edges

Applies whenever a **stored secret** exists for the host: a login **password** (password
auth) or a **key passphrase** (key auth with an encrypted key). `exec_connect` wires the
askpass env only when such a secret exists (`wire_askpass`); otherwise ssh prompts / uses the
agent normally, and in that no-secret case `configure_askpass` also **strips
`SSHELF_VAULT_PASSPHRASE`** from the child env (ssh has no reason to inherit the vault master
passphrase). In the wired case the variable must stay: the helper runs as ssh's child and
reads it to decrypt the vault (see `docs/security.md`).

`ssh` decides it needs a secret → because `SSH_ASKPASS_REQUIRE=force`, it executes the helper
as **`sshelf "<prompt text>"`** (the prompt is `argv[1]`; **there is no `--askpass` flag**).
The helper:

1. Confirms it's in askpass mode via `SSHELF_ASKPASS=1`.
2. **Inspects `argv[1]`** by OpenSSH prompt *shape* and branches:
   - Ends with `password:` (classic `user@host's password:` / PAM `Password:`) **or** contains
     `passphrase for` (`Enter passphrase for key '<path>':`) → fetch the secret for
     `SSHELF_HOST_ID` from `secrets` (keyring or age vault), print it, exit `0`.
   - Anything else (host-key `yes/no`, OTP/verification codes, arbitrary server text) →
     **exit non-zero** to decline, so `ssh` handles it. **Never blindly print the secret.**

A host uses one auth method, so answering both password and passphrase prompts with its one
stored secret is correct.

### Why inspection (by shape) is mandatory

`SSH_ASKPASS_REQUIRE=force` makes `ssh` route **every** `read_passphrase()` call to the
helper, including the first-connect *"Are you sure you want to continue connecting
(yes/no/fingerprint)?"*. If the helper answered that with the stored secret, the connection
breaks. Worse, a malicious/compromised server could use **keyboard-interactive** auth to send
a prompt that merely *mentions* "password" to phish the secret. Three defenses:

- The helper matches the **shape** of real prompts (ends-with `password:` / contains
  `passphrase for`) rather than a bare substring, so "Type your password to continue:" is
  declined.
- sshelf passes `-o StrictHostKeyChecking=accept-new`, so the host-key prompt never fires
  for new hosts (known hosts are still verified; changed keys still hard-fail).
- The secret is host-scoped, limiting blast radius even if a prompt is mis-answered.

### Validated by the M0 spike (2026-06-05, macOS, OpenSSH 10.2)

Ran against a real password-auth sshd (`lscr.io/linuxserver/openssh-server`):

- Success path: `SSH_ASKPASS=helper SSH_ASKPASS_REQUIRE=force`,
  `PreferredAuthentications=password`, `StrictHostKeyChecking=accept-new` → logged in (exit 0).
  Confirms `SSH_ASKPASS` satisfies interactive `PasswordAuthentication` as well as key
  passphrases.
  The helper was called with `argv[1] = "tester@127.0.0.1's password: "`.
- Host-key routing: with `StrictHostKeyChecking=ask` and a fresh `known_hosts`, ssh sent the
  helper the `"...continue connecting (yes/no/[fingerprint])?"` prompt; a naive helper that
  always returns the password caused an **infinite loop** on `"Please type 'yes', 'no'..."`.
  That is the empirical proof that §3's two rules are mandatory.

Linux verification is deferred to CI (M8); the mechanism is OpenSSH behavior and is
expected to be identical.

## 4. Known v1 limitations

- Password-auth jump hosts are unsupported. The helper only has the target's secret and
  can't tell which hop is prompting. Jump hosts must use key/agent auth in v1.
- macOS unsigned builds: the re-exec'd askpass child reading Keychain may trigger an OS
  approval prompt every connect (Keychain ACLs are keyed to code signature). Ad-hoc sign for
  dev; document for users building from source.
- Windows: out of scope for v1 (`exec()` replacement is Unix-only).

## References

- OpenSSH `ssh(1)`, `ssh_config(5)` (`ProxyJump`, `StrictHostKeyChecking`).
- `SSH_ASKPASS_REQUIRE`, added in OpenSSH 8.4 (2020). This machine runs 10.2.
- `std::os::unix::process::CommandExt::exec`.
