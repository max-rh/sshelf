# Passwords, keys & 2FA

> **Prefer SSH keys / agent where you can.** Password storage exists for hosts you can't use
> keys with; it is the least secure option sshelf offers. The full threat model:
> [Security](security.md).

## Auth methods

Each host uses one auth method, chosen in the [add/edit form](hosts.md):

- `agent` (default): ssh uses your keys/agent as usual and sshelf stores nothing.
- `key`: one or more `-i` identity files. If the key is encrypted, you can store its
  passphrase and sshelf supplies it automatically at connect.
- `password`: sshelf stores the login password and supplies it automatically.

## Where secrets live

- The OS keyring (default): macOS Keychain, or the Secret Service on Linux (GNOME Keyring /
  KWallet). Service `sshelf`, keyed by host id, or by `credential:<name>` for a
  [shared credential](#one-password-for-many-hosts).
- The age vault (headless): if `SSHELF_VAULT_PASSPHRASE` is set, secrets go to an
  `age`-encrypted file (`vault.age`, mode `0600`) instead, which is the path for servers and
  CI with no keyring daemon. The tradeoffs are documented in [Security](security.md).

Never in `hosts.toml`, never on a command line, never in logs or shell history.

## How auto-supply works

On connect, sshelf points `SSH_ASKPASS` at itself and `exec`s `ssh`. When ssh needs the
secret it invokes that helper, which answers **only** genuine password/passphrase prompts
(matched by their shape) and declines everything else, so a hostile server can't phish the
secret with a look-alike prompt, and the secret never appears in `ps` or on disk. The full
mechanics, and why this needs OpenSSH 8.4+, are in
[How the ssh command is built](ssh-command.md).

## Storing & changing a secret

- On the first connect: a host with nothing stored asks for it and keeps it once it works
  ([below](#saving-the-secret-on-first-connect)).
- In the form: the masked Password / Key passphrase field. When editing, blank keeps the
  existing secret.
- From a script or a headless box:

```sh
echo "$PASS" | sshelf set-password prod-db        # store or replace after the fact
echo "$PASS" | sshelf add legacy -H 10.0.0.9 -u root --password-stdin
```

Deleting a host removes its stored secret too, unless it was reading from a shared credential:
the other hosts on that credential still need it.

## One password for many hosts

When a set of hosts shares one login password (a directory password, an appliance fleet, a lab
full of identical boxes), storing it per host means retyping it once per host and again on every
rotation. A **credential** is that one secret, under a name, with hosts pointing at it:

```toml
format_version = 2

[[credential]]
name = "corp-ldap"
note = "the directory password"

[[host]]
name = "web-01"
hostname = "10.0.0.1"
auth = "password"
credential = "corp-ldap"
```

The file holds the pointer, never the secret, exactly as before. The secret lives in the keyring
or the vault under `credential:corp-ldap`, and every host bound to that name authenticates from
it. Rotating is one write:

```sh
printf %s "$PASS" | sshelf set-password --credential corp-ldap
```

From the CLI:

```sh
sshelf credentials add corp-ldap --note "the directory password"
sshelf credentials bind corp-ldap web-01 web-02 db-01
sshelf credentials                       # who uses what, and whether a secret is stored
sshelf credentials unbind web-02         # back to its own secret
```

In the TUI, `^s` marks hosts (the marked rows show a `◼`), and `^k` opens the credentials popup
for them. With nothing marked it acts on whatever the filter is showing, so `tag:prod` then `^k`
covers a whole group. The popup does three things:

One password, stored per host: the same value, written separately for each host. Hosts that
already have a secret are skipped by default, because replacing one is unrecoverable. `^r` turns
replacing on, and the count is on screen either way.

Shared credential: bind them all to a name and optionally set its secret. Leaving the password
field empty binds without touching a stored value.

Unbind: back to per-host secrets, with the credential itself kept.

A bulk write is **not checked against the hosts** (proving N secrets would mean N logins), so a
wrong one surfaces on that host's next connect with the usual
[refusal message](#when-a-stored-secret-is-wrong). The one-host path still checks, because it
can. Setting a password on a host that is bound updates the shared credential, and both the CLI
and the status line say how many hosts that affects.

Naming a credential no host has declared still works: the `[[credential]]` table is metadata
(the note, and later where the value comes from), not the lookup. `sshelf credentials` flags
one as undeclared so a typo is visible rather than silent.

A file that uses credentials declares `format_version = 2`. A file that does not keeps
declaring 1, so adopting 0.16 does not by itself produce a file an older sshelf would refuse.

## Saving the secret on first connect

A password host with nothing stored, or a key host whose key needs a passphrase, asks for it on
the terminal when you connect. In the TUI that happens after the list is gone and before ssh
starts:

```
Password for ubuntu@44.196.235.116 (saved to your keyring once it works; Enter to skip):
```

It reads with echo off (`vault` instead of `keyring` when you use the vault). Then:

1. The answer is stored right away, because the askpass helper reads it from the store and
   nothing else can get it to ssh without putting it in argv.
2. sshelf proves it with one throwaway `ssh ... exit` against the host. Exit status 255 is ssh's
   own failure; anything else means ssh got in and ran `exit`.
3. If it worked, you see `saved password for <name>` and the real connect goes ahead. Every later
   connect goes straight in.
4. If the server refused it, the secret is removed again, sshelf prints
   `the password was refused by <user@host>; nothing saved`, and exits 1. Any other failure
   (unreachable, no answer in 30 seconds) also removes it: a secret that couldn't be checked
   isn't kept.

`Enter` on the empty prompt skips. The connect runs exactly as before and ssh asks for itself.
Nothing is remembered, so the next connect asks again, until something is stored. `Esc` or
`Ctrl-C` backs out without connecting. There's no setting to turn the question off; skipping is
the way out.

A key host gets one more step first. Each identity file is checked with `ssh-keygen -y`, and only
a key that needs a passphrase counts. sshelf then tries the host once with `BatchMode=yes`. If
your agent, or an unencrypted key next to the encrypted one, already gets you in, nothing is asked
and nothing is stored. Only a refusal from the server leads to the prompt. If the host can't be
reached at all, the connect goes ahead and ssh shows the real error, since no passphrase would fix
that.

A first connect costs one extra handshake, two for an encrypted key. Every connect after it is
the same as before.

Agent hosts, hosts that already have a secret, and a host behind two or more jump hosts are never
asked. The last kind wires no helper at all, so there's nothing to save into, and ssh asks on the
terminal as it always has. `sshelf print-command` and `sshelf list --json` never ask either.

2FA hosts are saved without the check, because checking would use up the code the host is about
to ask for. The password comes first and the code second, since the code is the one that goes
stale, and the line says it wasn't checked:
`saved password for <name> (not checked: this host needs a code; if the login fails, press ^e in
the TUI or run sshelf set-password <name>)`. In the TUI, a 2FA host that's about to be asked
skips the code popup, and both questions come on the terminal in that order.

In [tmux mode](search-connect.md#connecting-inside-tmux), a host that would be asked connects in
place instead of in a new window, because the question needs this terminal.

## When a stored secret is wrong

ssh asks again after a refused password or passphrase, and the helper would hand over the same
wrong value every time until ssh gave up, which reads like the server turning you away. So the
helper notices when ssh asks the same question twice in one connect, which only happens after a
refusal, and says so:

```
sshelf: the stored password for 01J9ZK... was refused; replace it with sshelf set-password or ^e in the TUI
```

Then it declines, so ssh stops retrying with that value. The stored secret is **not** deleted: a
server can ask twice for reasons of its own, and a helper that deleted on a repeat could throw
away a correct one. Replace it with `^e` or `sshelf set-password <name>`. The transfer screen and
port forwards say `the stored password was refused` when they fail this way.

## Two-factor (2FA) hosts

Some servers ask for a verification code (TOTP / keyboard-interactive) on top of your key or
password. Set **2FA = yes** on the host (form, or `sshelf add ... --2fa`):

- TUI connect: a popup collects the current code *before* the ssh handoff and feeds it to
  the server's verification prompt through the same askpass channel. sshelf never proxies the
  live session.
- CLI connect (`sshelf <host>`): prompts for the code on the terminal.

The code is masked either way. The popup shows one bullet per character, like the password
field in the host form, and the terminal prompt reads with echo off so the code never lands in
scrollback or a screen recording. `Esc` or `Ctrl-C` backs out without connecting. If stdin is a
pipe rather than a terminal, the old line read is used instead, so a script can still feed the
code in.

Codes are manual entry: sshelf does not store TOTP seeds. The flag exists because a
connect that auto-supplies a stored secret runs ssh with `SSH_ASKPASS_REQUIRE=force`, which
routes the code prompt to the helper with **no terminal fallback**. Unflagged, such a
connect fails at the code prompt. (A host with no stored secret is asked for one on its first
connect, [above](#saving-the-secret-on-first-connect); a host that combines an encrypted key with
2FA is better served by the agent.) Background: [`decisions.md`](decisions.md), D-022.

## Limitations worth knowing

- Jump hosts must use key/agent auth. The askpass helper only holds the *target's* secret and
  can't tell which hop is prompting, so with a secret in play sshelf either constrains a single
  hop to key/agent auth explicitly or hands it no helper at all. See the
  [FAQ](faq.md#can-a-jump-host-use-password-auth).
- A key host is connected with `PreferredAuthentications=publickey` (plus
  `keyboard-interactive` when it needs a code), so a server cannot fall back to asking for a
  password. If one of your key hosts relied on that fallback, add the password as a second host
  or change that host's auth to password.
- OpenSSH prints at most 100 characters of a key's path in its passphrase prompt, and the helper
  only answers a prompt that names one of the host's key files exactly. A passphrase for a key
  whose full path is longer than that can't be supplied, and the first connect doesn't ask for
  one. Keep that key in your agent, or move it somewhere with a shorter path.
- A shared credential is one secret, so every host on it has to accept the same one. There is no
  per-host override: a host that needs its own password is unbound instead.
- sshelf 0.15.0 and earlier do not know the `credential` field or the `[[credential]]` table.
  Running an older build against a file that uses them drops both on its next save, which
  unbinds the hosts (their secrets survive, under the credential's key). 0.16.0 onwards refuses
  to load a file that declares a newer `format_version` rather than rewriting it, so this is the
  last version where that can happen.
- Building from source on macOS: an unsigned binary may trigger a Keychain approval
  prompt on connect (Keychain ACLs are keyed to the code signature). See the
  [FAQ](faq.md#password-auto-supply-isnt-working).
