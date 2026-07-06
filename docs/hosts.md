# Adding & editing hosts

`Ctrl-a` opens the add form; `Ctrl-e` edits the selected host. It's a single screen — every
field shows a dim placeholder explaining it (`required ·` for Name/Hostname, `optional ·`
elsewhere), and it's **auth-aware**: only the fields relevant to the chosen Auth method are
shown.

**Quick-add:** the form opens with sensible defaults, so a Name + Hostname and `Ctrl-s` is
enough.

## Fields

Always shown: **Name** (required), **Hostname** (required), **User** (defaults to `$USER` at
connect time), **Port** (defaults 22), **Auth**, **Jump hosts** (ProxyJump chain — key/agent
auth only), **Tags**, **Site**, **2FA** (`←`/`→` yes/no — prompt for a verification code on
connect), **Extra args** (raw ssh flags appended verbatim — the escape hatch for anything the
form doesn't model, e.g. `-X` or `-o ServerAliveInterval=30`).

Auth-specific fields:

| Auth | Extra fields |
|---|---|
| `agent` (default) | none — ssh uses your agent/keys as usual |
| `key` | **Key** — `←`/`→` cycles private keys found in `~/.ssh`; `Enter` opens a file browser to pick a key anywhere. **Key passphrase** — optional, only if the key is encrypted |
| `password` | **Password** — stored in the OS keyring / vault, never in a file |

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
keyed by host id — **never** into `hosts.toml`. When editing, leaving the field blank keeps
the existing secret. Details: [Passwords, keys & 2FA](passwords-2fa.md).

## Deleting

`Ctrl-d` on the selected host asks for confirmation (`y`), then removes the host, its
frecency history, and its stored secret.

## Prefer the command line?

Everything above can be done non-interactively with `sshelf add` — see
[Adding hosts from the CLI](cli.md#adding-hosts-from-the-cli). `hosts.toml` itself is
designed to be hand-edited too; the full schema is in [Data model & files](data-model.md)
(that's also how you give one host **multiple** identity files).
