# CLI reference

Everything sshelf does without opening the TUI.

## Commands

| Command | What it does |
|---|---|
| `sshelf` | Launch the interactive TUI. |
| `sshelf <host>` | Connect straight to a saved host by **name or id**, skipping the TUI — same connect path as `Enter` (frecency recorded, stored secret auto-supplied, [2FA code](passwords-2fa.md#two-factor-2fa-hosts) prompted on the terminal). A miss suggests the closest names; a host named like a subcommand (`list`, `import`, …) is reached via the TUI instead. |
| `sshelf -` | Reconnect to the most recently used host. Errors (without connecting) if there's no history yet. |
| `sshelf add [NAME …]` | Bare: open the TUI add form. With arguments: add a host non-interactively — see [below](#adding-hosts-from-the-cli). |
| `sshelf list [query] [--json]` | List hosts (with a `·site·` column). `query` filters with the TUI's syntax — fuzzy text and/or `tag:NAME` / `site:NAME` (e.g. `sshelf list site:prod-dc`). |
| `sshelf print-command <host>` | Print the generated, shell-quoted `ssh …` command (site defaults included) without connecting or touching frecency — the CLI twin of `Ctrl-y`. |
| `sshelf sites [--json]` | List defined sites with member counts + their shared defaults. |
| `sshelf sites add NAME [-u/-p/-J/-i]` | Define a [site](sites-tags.md) (settings optional; edit later with `F3`). |
| `sshelf import [--dry-run]` | [Read-only import](import.md) from `~/.ssh/config`. |
| `sshelf export [--stdout]` | [Export](export.md) the database as an ssh_config `Include` fragment, written next to sshelf's config (`--stdout` prints it instead). Once the file exists, it refreshes on every hosts change. |
| `sshelf set-password <host>` | Store a password / key passphrase for a host, read from **stdin**. |
| `sshelf completions <shell>` | Print static shell completions. |
| `sshelf man` | Print the man page. |

Global flags: **`--config FILE`** — use a specific config file (also `$SSHELF_CONFIG`); see
[Configuration](configuration.md). **`--transfer-log FILE`** — append transfer diagnostics,
no secrets, to FILE (also `$SSHELF_TRANSFER_LOG`); see
[Transferring files](transfer.md#debugging-a-failing-transfer).

## Adding hosts from the CLI

`sshelf add` with arguments adds a host non-interactively (handy for scripts and dotfiles).
`NAME` and `--hostname` are required:

```sh
# key auth (a -i identity implies --auth key)
sshelf add prod-web -H 10.25.25.10 -u deploy -i ~/.ssh/id_ed25519 -t prod,web

# jump host + custom port
sshelf add prod-db -H 10.25.25.25 -u mike -p 5432 -J bastion.example.net -i ~/.ssh/db

# agent auth (the default) with extra raw ssh flags
sshelf add edge -H edge.example.net --extra "-o ServerAliveInterval=30"

# password auth — pipe the secret in (kept out of argv/history; stored in the keyring or vault)
echo "$PASS" | sshelf add legacy -H 10.0.0.9 -u root --password-stdin
```

| Flag | Meaning |
|---|---|
| `NAME` (positional) | Alias to search/connect by. **Required.** |
| `-H, --hostname <HOST>` | IP or DNS name. **Required.** |
| `-u, --user <USER>` | Login user (defaults to `$USER` at connect time). |
| `-p, --port <PORT>` | SSH port (default 22). |
| `-a, --auth <agent\|key\|password>` | Auth method. Inferred as `key` with `--identity`, `password` with `--password-stdin`, else `agent`. |
| `-i, --identity <PATH>` | Identity file for key auth (repeatable). |
| `-J, --jump <HOST>` | ProxyJump host (repeatable or comma-separated). Key/agent auth only. |
| `-t, --tag <TAG>` | Tag (repeatable or comma-separated). |
| `-s, --site <NAME>` | Assign the host to a [site](sites-tags.md). |
| `--2fa` | Mark the host as needing a [verification code](passwords-2fa.md#two-factor-2fa-hosts) on connect. |
| `--extra "<ARGS>"` | Extra raw ssh flags, appended verbatim. |
| `--password-stdin` | Read a password / key passphrase from stdin and store it. |

A duplicate name is refused. To store a secret after the fact, use
`sshelf set-password <name>`.

## Shell completions

**Static** completion (subcommands + flags) ships with every package — open a new shell after
installing so it loads. It's also printed by `sshelf completions <shell>`.

**Dynamic** completion — `sshelf prod<Tab>` completing your saved **host names** — takes one
line in your shell rc:

```sh
source <(COMPLETE=bash sshelf)   # bash — add to ~/.bashrc
source <(COMPLETE=zsh sshelf)    # zsh  — add to ~/.zshrc (after compinit)
COMPLETE=fish sshelf | source    # fish — add to ~/.config/fish/config.fish
```

Host-name completion works for direct connect, `print-command`, and `set-password`.

## JSON output

`sshelf list --json [query]` emits each selected host's fields **plus its generated `ssh`
command**, and is always valid JSON even when the selection is empty — the stable surface for
scripts and integrations. `sshelf sites --json` does the same for sites.
