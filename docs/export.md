# Exporting to SSH config

`sshelf export` makes your sshelf hosts available to **everything else**: it writes an
ssh_config fragment to sshelf's own file and you add a single `Include` line to your
`~/.ssh/config` — sshelf never edits that file (or anything under `~/.ssh`).

```sh
sshelf export
# Exported 14 host(s) to ~/.config/sshelf/ssh_config
# To use it, add this line to your ~/.ssh/config (sshelf never edits that file):
#   Include ~/.config/sshelf/ssh_config
```

Once included, sshelf's database stops being a walled garden — your hosts resolve **by name**
in any tool that reads SSH config:

```sh
ssh prod-web                          # plain ssh — no sshelf in the loop
scp report.pdf prod-web:/tmp/         # scp / sftp
rsync -av ./site/ prod-web:/var/www/  # rsync (runs over ssh)
git clone prod-web:/srv/repo.git      # git's ssh transport
```

…and in anything with an SSH-config picker: **VS Code Remote-SSH** lists your sshelf hosts in
its host dropdown, JetBrains Gateway and similar tools likewise. The jump host, port, user,
and identity file all come along — `ssh prod-web` through the site's bastion just works.

## Staying fresh

Creating the file once (running `sshelf export`) is the opt-in: from then on **sshelf
rewrites it automatically every time your hosts change** — add/edit/delete in the TUI,
`sshelf add`, an import, a site change. Edit a host's bastion in sshelf and VS Code picks it
up on its next connect. (Delete the file to opt back out; `sshelf export --stdout` prints the
fragment without writing anything.)

The output is **deterministic** — hosts sorted by name, no timestamps — so the file only
changes when your database does. Diff-friendly if you keep it alongside dotfiles.

## What gets exported

Per host (with its [site defaults](sites-tags.md) resolved, exactly like connect):
`HostName`, `User`, `Port` (when not 22), `IdentityFile` (key-auth hosts; `~` left for ssh to
expand), and `ProxyJump`. From **Extra args**, `-o Key=Value` options translate to real
config directives; other raw flags (`-X`, …) can't be expressed in config and are kept
visible as a comment in the host's block instead of being guessed at.

Worth knowing:

- **Where sshelf's entries win.** ssh uses the *first* value it finds for an option, so put
  the `Include` **at the top** of `~/.ssh/config` if sshelf's entries should win for
  same-named hosts, or at the bottom if your hand-written entries should.
- **Password-auth hosts** export fine, but plain `ssh` can't read sshelf's keyring — it
  prompts on the terminal. Auto-supply (and the [2FA popup](passwords-2fa.md)) remain
  sshelf-connect features.
- **No behavior smuggling.** sshelf's own connects pass `StrictHostKeyChecking=accept-new`
  (an [askpass necessity](ssh-command.md)); the export deliberately does *not*, so your plain
  ssh keeps your own host-key defaults.
- **Names that can't be Host patterns** (containing `*`, `?`, `!`, `,`, `#`, or quotes) are
  skipped with a comment — they'd otherwise match *other* hostnames. Names with spaces are
  quoted and work.

Round-tripping with [import](import.md) is symmetric on purpose: import copies your SSH
config **in** (read-only), export projects the database **out** (to its own file). Your
`~/.ssh/config` is never written by either. Design notes: [`decisions.md`](decisions.md),
D-023.
