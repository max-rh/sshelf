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
    | -o ProxyCommand=...            # instead of -J, for one hop with a secret to protect (§3a)
  -o StrictHostKeyChecking=accept-new   # see §3: keeps host-key prompt away from askpass
  [-o PreferredAuthentications=publickey[,keyboard-interactive]]   # key hosts only (§3)
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

Both resolve the host's stored secret first, so what you copy is the command sshelf would
actually run, including the `ProxyCommand` a jump host gets when a secret is in play (§3a).
`sshelf list --json` is the exception: its `command` field is built as if no secret were
stored, because a listing must not read the keyring once per host. That is also the right
answer for a command you run by hand, which has no askpass helper for a hop to inherit.

## 2. Launch handoff (`exec`)

On connect:

1. **Persist frecency first** (`exec()` never returns, so nothing runs after it).
2. Set environment for the child:
   - `SSH_ASKPASS = <path to sshelf's own binary>` (`std::env::current_exe()`)
   - `SSH_ASKPASS_REQUIRE = force`
   - `SSHELF_ASKPASS = 1`        ← how the re-exec'd binary knows it's in askpass mode
   - `SSHELF_HOST_ID = <id>`     ← which secret to fetch
   - `SSHELF_SECRET_KIND = password | passphrase | agent`  ← which secret that id holds (§3)
   - `SSHELF_IDENTITY_FILES = <path>[:<path>...]`  ← key hosts only, `~` already expanded (§3)
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
  [-e SSHELF_SECRET_KIND=...] [-e SSHELF_IDENTITY_FILES=...]
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
2. Reads `SSHELF_SECRET_KIND`, which says whether the value behind `SSHELF_HOST_ID` is a login
   `password`, a key `passphrase`, or nothing at all (`agent`). A missing or unrecognised kind
   declines everything.
3. **Inspects `argv[1]`** by OpenSSH prompt *shape*, and answers only the shape that matches
   its own kind:
   - Ends with `password:` (classic `user@host's password:` / PAM `Password:`) and the kind is
     `password` → fetch the secret for `SSHELF_HOST_ID` from `secrets` (keyring or age vault),
     print it, exit `0`.
   - Looks exactly like OpenSSH's local key prompt, `Enter passphrase for key '<path>':`, the
     kind is `passphrase`, **and** `<path>` is one of the paths in `SSHELF_IDENTITY_FILES` →
     same, print the secret and exit `0`.
   - A secret-shaped prompt of the *other* kind, or a passphrase prompt naming a key this host
     does not use → **exit non-zero** to decline. It is never answered with the queued code
     either.
   - Anything else (host-key `yes/no`, OTP/verification codes, arbitrary server text) → the
     one-time code in `SSHELF_2FA_CODE` when one was queued for this connection, otherwise
     **exit non-zero** to decline. **Never blindly print the secret.**

### Why shape alone is not enough

`SSH_ASKPASS_REQUIRE=force` makes `ssh` route **every** `read_passphrase()` call to the
helper, including the first-connect *"Are you sure you want to continue connecting
(yes/no/fingerprint)?"*. If the helper answered that with the stored secret, the connection
breaks.

The prompt text of a keyboard-interactive round is written by the **server**, and `Password:`
is a perfectly well-shaped prompt. So a host that rejects your key can ask for a password over
keyboard-interactive and, before 0.14.0, be handed the key's passphrase. That is why the kind
is passed in and why a key host is also told which key files are in play: the only passphrase
prompt it will answer is OpenSSH's own, naming a path it was given with `-i`. Defenses, in
order of how much they carry:

- The helper answers only prompts of its own kind, and a key host only for its own key files.
- Key hosts pass `-o PreferredAuthentications=publickey`, so the server cannot offer password
  auth at all. A key host that also needs a verification code passes
  `publickey,keyboard-interactive`, since that is how the code arrives.
- The helper matches the **shape** of real prompts rather than a bare substring, so "Type your
  password to continue:" is not treated as a secret prompt.
- sshelf passes `-o StrictHostKeyChecking=accept-new`, so the host-key prompt never fires
  for new hosts (known hosts are still verified; changed keys still hard-fail).
- The secret is host-scoped, limiting blast radius even if a prompt is mis-answered.

### 3a. The jump hop never sees the helper

`ssh` starts the `ProxyJump` hop as a child process, so it inherits `SSH_ASKPASS` and the rest
of the wiring. It does **not** forward the destination's `-o` options to that hop: only `-l`,
`-p`, `-J`, `-F` and `-v` cross over. Nothing on the target's command line constrains the hop,
so a hostile or compromised bastion could ask for a password and be handed the target's stored
secret. What sshelf does instead, whenever the helper would be wired at all (a stored secret,
or a queued verification code):

- **One jump host**, and the string is made only of `A-Za-z0-9._@:[]-`: drop `-J` and pass

  ```
  -o ProxyCommand=ssh -o BatchMode=yes -o PasswordAuthentication=no \
     -o KbdInteractiveAuthentication=no [-l USER] [-p PORT] -W '[%h]:%p' JUMP
  ```

  `USER`, `PORT` and `JUMP` come from the stored `user@host:port`. `BatchMode=yes` on its own
  disables password prompts; the two explicit `no`s are there so the rule does not rest on one
  reading of the man page. A hop reached this way can authenticate with an agent or an
  unencrypted key file and nothing else, which is what the FAQ always said jump hosts had to
  be. The allowlist is narrow because `ssh` runs a `ProxyCommand` through your shell.
- **Two or more hops**, or one that does not parse or does not pass the allowlist: `-J` stays
  exactly as stored and **nothing** is wired. `ssh` asks for the target's secret on the
  terminal, and sshelf prints `multi-hop jump with a stored secret: ssh will ask for it on the
  terminal` first so that is not a surprise. In tmux mode the connection falls back to the
  in-place handoff for the same reason: a new window has no terminal to ask on.
- **Nothing stored and no code queued** (agent hosts, key hosts with an unencrypted key):
  `-J` is untouched. There is no helper for a hop to inherit.

`master_args` (the transfer ControlMaster) and `build_forward_command` (port forwards) build
their argv through the same function, so they get the same treatment.

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

- Password-auth jump hosts are unsupported, and since 0.14.0 that is enforced rather than
  only documented (§3a). The helper only has the target's secret and can't tell which hop is
  prompting, so a hop is either constrained by an explicit `ProxyCommand` or gets no helper at
  all. Jump hosts must use key/agent auth.
- macOS unsigned builds: the re-exec'd askpass child reading Keychain may trigger an OS
  approval prompt every connect (Keychain ACLs are keyed to code signature). Ad-hoc sign for
  dev; document for users building from source.
- Windows: out of scope for v1 (`exec()` replacement is Unix-only).

## References

- OpenSSH `ssh(1)`, `ssh_config(5)` (`ProxyJump`, `StrictHostKeyChecking`).
- `SSH_ASKPASS_REQUIRE`, added in OpenSSH 8.4 (2020). This machine runs 10.2.
- `std::os::unix::process::CommandExt::exec`.
