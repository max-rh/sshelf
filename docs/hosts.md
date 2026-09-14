# Adding & editing hosts

`Ctrl-a` opens the add form; `Ctrl-e` edits the selected host. It's a single screen. Every
field shows a dim placeholder explaining it (`required ·` for Name/Hostname, `optional ·`
elsewhere), and it's **auth-aware**: only the fields relevant to the chosen Auth method are
shown.

**Quick-add:** the form opens with sensible defaults, so a Name + Hostname and `Ctrl-s` is
enough.

## Add a host from an ssh command

If you already have a working `ssh` line, from your history or a runbook, sshelf can read it:

```sh
sshelf add --from-ssh 'ssh -i ~/Downloads/dev-ooblek-privco.pem -o StrictHostKeyChecking=no ubuntu@44.196.235.116'
```

That opens this form filled in: `44.196.235.116` as the name and the hostname, `ubuntu` as the
user, auth `key` with that key file. Change what you like and save, or `Esc` to add nothing.
Focus starts on the name, or on the password for a password host, since a command line never
carries one. `--quiet` saves the host without the form. To save the command you just ran:

```sh
sshelf add --from-ssh "$(fc -ln -1)"
```

`ssh ... | sshelf add --from-ssh` can't work: the shell runs that `ssh` and pipes its output.
Hand sshelf the command as text instead, as above, or with `echo 'ssh ...' |` in front.

The line is read with ssh's own option rules, so `-At`, `-p2222`, `--` and options after the
destination all mean what they mean to ssh. Nothing is looked up: an alias from your
`~/.ssh/config` stays an alias, and ssh still resolves it when you connect.

| On the line | Becomes |
|---|---|
| `user@host`, `ssh://user@host:port` | hostname, user, port |
| `-l USER`, `-p PORT` | user and port, over the ones in the destination |
| `-i KEY` (repeatable) | identity files, and auth `key`. A relative path is saved absolute, since you'll connect from other directories. |
| `-J a,b` | jump hosts |
| `-o PasswordAuthentication=yes` or `-o PreferredAuthentications=password`, with no `-i` | auth `password`. Those two options aren't kept. |
| anything else, e.g. `-A`, `-L ...`, `-F ...`, `-o ServerAliveInterval=30` | extra args, in order |

A few options are dropped, each with a line saying why: `-v`, `-q`, `-G`, `-V`, `-Q`, `-O`, `-S`,
`-E`, `-M`, `-N`, `-f`, `-n`, `-g`, `-s`, and `-o StrictHostKeyChecking=...`. That last one
because sshelf passes `accept-new` on every connect and ssh keeps the first value it sees, so a
saved `no` would claim something that isn't true. A remote command at the end of the line is
refused rather than dropped, since a saved host has no command, and so is anything that isn't
ssh in front (`sudo ssh ...`). The name defaults to the destination's host; when that name is
taken, give one first: `sshelf add prod-web --from-ssh '...'`.

## Fields

Always shown: **Name** (required), **Hostname** (required), **User** (defaults to `$USER` at
connect time), **Port** (defaults 22), **Auth**, **Jump hosts** (ProxyJump chain, key/agent
auth only), **Tags**, **Site**, **2FA** (`←`/`→` yes/no, prompts for a verification code on
connect), **Extra args** (raw ssh flags appended verbatim, the escape hatch for anything the
form doesn't model, e.g. `-X` or `-o ServerAliveInterval=30`).

Auth-specific fields:

| Auth | Extra fields |
|---|---|
| `agent` (default) | none, ssh uses your agent/keys as usual |
| `key` | **Key**: `←`/`→` cycles private keys found in `~/.ssh`; `Enter` opens a file browser to pick a key anywhere. **Key passphrase**: optional, only if the key is encrypted |
| `password` | **Password**: stored in the OS keyring / vault, never in a file |

Key discovery finds keypairs (a `.pub` sibling) **and** standalone private keys including
`.pem` (detected by their `PRIVATE KEY` header), so AWS-style keys show up too.

**The file browser** (from the Key field with `Enter`): type to fuzzy-filter, `↑`/`↓` move,
`Enter` opens a directory or selects a file, `←` goes up, `Backspace` edits the filter (or
goes up when it's empty), `Esc` clears the filter (or cancels when it's empty). It starts in
`~/.ssh` (or near the current key); a picked key can live anywhere.

## Navigating the form

`Tab` / `↑` / `↓` move between fields · `←` / `→` (or space) change the choosers (Auth, Key,
Site, 2FA) · `Enter` advances and **saves on the last field** · `Ctrl-s` saves from anywhere ·
`Esc` cancels. Validation errors (missing name/hostname, non-numeric port) show inline, and
focus jumps to the offending field.

## Secrets in the form

The masked **Password** / **Key passphrase** value goes to the OS keyring (or the age vault)
keyed by host id, **never** into `hosts.toml`. When editing, leaving the field blank keeps
the existing secret. Details: [Passwords, keys & 2FA](passwords-2fa.md).

## Deleting

`Ctrl-d` on the selected host asks for confirmation (`y`), then removes the host, its
frecency history, and its stored secret.

## Prefer the command line?

Everything above can be done non-interactively with `sshelf add`. See
[Adding hosts from the CLI](cli.md#adding-hosts-from-the-cli). `hosts.toml` itself is
designed to be hand-edited too; the full schema is in [Data model & files](data-model.md)
(that's also how you give one host **multiple** identity files). Each host needs an `id`, and any
string that's unique in the file will do: sshelf only uses it to find the host's secret and its
usage history. Secrets never go in the file, so a password host (or an encrypted-key host) you
wrote by hand has nothing stored. Its first connect asks for the secret and saves it once it
works; see [saving the secret on first connect](passwords-2fa.md#saving-the-secret-on-first-connect).
