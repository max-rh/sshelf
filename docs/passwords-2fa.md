# Passwords, keys & 2FA

> **Prefer SSH keys / agent where you can.** Password storage exists for hosts you can't use
> keys with; it is the least secure option sshelf offers. The full threat model:
> [Security](security.md).

## Auth methods

Each host uses one auth method, chosen in the [add/edit form](hosts.md):

- **`agent`** (default) — ssh uses your keys/agent as usual; sshelf stores nothing.
- **`key`** — one or more `-i` identity files. If the key is **encrypted**, you can store its
  passphrase and sshelf supplies it automatically at connect.
- **`password`** — sshelf stores the login password and supplies it automatically.

## Where secrets live

- **OS keyring** (default): macOS Keychain, or the Secret Service on Linux (GNOME Keyring /
  KWallet). Service `sshelf`, keyed by host id.
- **The age vault** (headless): if `SSHELF_VAULT_PASSPHRASE` is set, secrets go to an
  `age`-encrypted file (`vault.age`, mode `0600`) instead — the path for servers and CI with
  no keyring daemon. The tradeoffs are documented in [Security](security.md).

Never in `hosts.toml`, never on a command line, never in logs or shell history.

## How auto-supply works

On connect, sshelf points `SSH_ASKPASS` at itself and `exec`s `ssh`. When ssh needs the
secret it invokes that helper, which answers **only** genuine password/passphrase prompts
(matched by their shape) and declines everything else — so a hostile server can't phish the
secret with a look-alike prompt, and the secret never appears in `ps` or on disk. The full
mechanics — and why this needs OpenSSH 8.4+ — are in
[How the ssh command is built](ssh-command.md).

## Storing & changing a secret

- **In the form:** the masked Password / Key passphrase field. When editing, blank keeps the
  existing secret.
- **From a script / headless:**

```sh
echo "$PASS" | sshelf set-password prod-db        # store or replace after the fact
echo "$PASS" | sshelf add legacy -H 10.0.0.9 -u root --password-stdin
```

Deleting a host removes its stored secret too.

## Two-factor (2FA) hosts

Some servers ask for a verification code (TOTP / keyboard-interactive) on top of your key or
password. Set **2FA = yes** on the host (form, or `sshelf add … --2fa`):

- **TUI connect:** a popup collects the current code *before* the ssh handoff and feeds it to
  the server's verification prompt through the same askpass channel. sshelf never proxies the
  live session.
- **CLI connect** (`sshelf <host>`): prompts for the code on the terminal.

Codes are **manual entry** — sshelf does not store TOTP seeds. The flag exists because a
connect that auto-supplies a stored secret runs ssh with `SSH_ASKPASS_REQUIRE=force`, which
routes the code prompt to the helper with **no terminal fallback** — unflagged, such a
connect fails at the code prompt. (A host with no stored secret prompts inline anyway; a host
that combines an encrypted key with 2FA but no stored passphrase is better served by the
agent.) Background: [`decisions.md`](decisions.md), D-022.

## Limitations worth knowing

- **Jump hosts must use key/agent auth.** The askpass helper only holds the *target's* secret
  and can't tell which hop is prompting.
- **Building from source on macOS:** an unsigned binary may trigger a Keychain approval
  prompt on connect (Keychain ACLs are keyed to the code signature) — see the
  [FAQ](faq.md#password-auto-supply-isnt-working).
