# Importing from ~/.ssh/config

`sshelf import` (or `Ctrl-o` in the TUI) copies hosts from `~/.ssh/config` into sshelf's own
database. It is **strictly read-only**: sshelf parses the file and never writes back — your
SSH config is not touched, ever.

```sh
sshelf import --dry-run    # preview what would be imported
sshelf import              # import
```

What it does:

- adds every host whose **name isn't already present** in sshelf — re-running is safe,
  existing names are left alone;
- carries over the fields sshelf models (hostname, user, port, identity files);
- **warns** about directives it doesn't import — `Match`, `Include`, `ProxyJump` — instead of
  silently mis-importing them.

Import brings everything in at once (there's no per-host picker); curate afterwards with
`Ctrl-e` / `Ctrl-d`, and organize with tags or [sites](sites-tags.md). Your `~/.ssh/config`
keeps working exactly as before — sshelf's database is independent of it by design (the
[FAQ](faq.md#why-doesnt-sshelf-just-use-my-ssh-config) explains why).
