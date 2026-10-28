# Shell integration

Three ways to reach a host without opening the full TUI: tab completion, a picker bound to a
key, and `sshelf pick` inside your own scripts.

## Tab completion

`sshelf prod<Tab>` completes your saved host names once dynamic completion is set up, which is
one line in your shell rc. See [Shell completions](cli.md#shell-completions).

## A picker on a key

`contrib/sshelf.plugin.zsh` (zsh) and `contrib/sshelf.bash` (bash) bind **Alt-s** to the host
picker and drop what you choose onto the command line you are already typing.

| Where you press it | What you get |
|---|---|
| empty line | `sshelf web-01`, ready to run |
| mid-line, e.g. after `scp file ` | the host name inserted at the cursor |
| with rows marked (`^s` in the picker) | every marked name, space separated |

Backing out of the picker with `Esc` leaves your line exactly as it was.

Install it with whatever you already use:

```sh
# Oh My Zsh: copy contrib/ in as a plugin directory, then add sshelf to plugins=()
cp -r contrib "$ZSH_CUSTOM/plugins/sshelf"

# zinit
zinit ice as'plugin' pick'contrib/sshelf.plugin.zsh'; zinit load max-rh/sshelf

# antidote (.zsh_plugins.txt)
max-rh/sshelf path:contrib

# or plainly, in ~/.zshrc or ~/.bashrc
source /path/to/sshelf/contrib/sshelf.plugin.zsh
```

Settings, set before sourcing: `SSHELF_BIN` is the binary to call, `SSHELF_PICK_KEY` the
binding. `^s` is free in zsh only after `stty -ixon`, since the terminal claims it for flow
control, and Alt needs Option-as-Meta turned on in Terminal.app. The bash key binding needs
bash 4.0 or newer; on the 3.2 Apple ships, the file defines `sshelf-pick` as a plain command
instead.

## `sshelf pick` on its own

`sshelf pick` opens the list in the rows under your prompt, prints the chosen host's **name** on
stdout, and connects to nothing. Your prompt is still there when it closes, and nothing it drew
is left behind:

```sh
host=$(sshelf pick)      # the list draws on your terminal, the name lands in $host
sshelf "$host"           # connect when you are ready
sshelf pick tag:prod     # start with the search box filled
```

The streams are the contract. The list is drawn on **stderr**, so stdout carries the name and
nothing else. Quitting without choosing prints nothing and **exits 1**, which is how a script
tells "backed out" from "chose a host". Mark rows with `^s` and every marked name is printed, one
per line.

It draws 15 rows by default. `--height ROWS` changes that for one run, `pick_height` in
[`config.toml`](configuration.md) changes it for good, and either set to `0` (or
`--full-screen`) takes the whole terminal the way the TUI does. A height taller than the window
is clamped so your prompt keeps a row.

Pick mode answers only the keys a chooser needs: type to filter, `↑`/`↓` or `^p`/`^n` to move,
`^s` to mark, `Enter` to print, `Esc` to back out, `^c` to quit. The keys that add, edit, delete
or open another screen do nothing, partly because the add form and the managers are full-screen
designs that will not fit in fifteen rows, and partly because something your shell fires off on
a keypress should not be able to edit the database on the way past.

Nothing is recorded in the frecency state by a pick, because the connect that normally follows
records its own use.

A few things that fall out of it:

```sh
# run something on every host you marked
for h in $(sshelf pick); do sshelf "$h" -- uptime; done

# the full ssh command for a host you point at
eval "$(sshelf print-command "$(sshelf pick)")"

# pick, then open the transfer screen on that host
sshelf "$(sshelf pick)"
```

`sshelf pick` prints the name sshelf knows the host by, which `ssh` and `scp` do not resolve on
their own. Either go through `sshelf` as above, or
[export an ssh_config fragment](export.md) once and then plain `scp file web-01:/tmp/` works
too.

## With fzf instead

If you would rather filter with fzf, `list --json` is the stable surface and needs no picker at
all:

```sh
sshelf list --json | jq -r '.[].name' | fzf
```

That loses the site sections, the match highlighting and the frecency order, and it reads the
whole database up front. It is the right tool if you are already composing fzf pipelines.

## Writing your own integration

`sshelf pick` is meant to be called from a widget, so two details matter.

Give it a terminal on stderr. A zsh widget keeps all three streams for itself, so the
command needs `2>/dev/tty`, which is what the shipped plugin does. Without a terminal there,
pick refuses rather than drawing into whatever stderr happens to be.

It will find its own keyboard. The picker reads keys from the controlling terminal even when
stdin is a pipe or the shell has taken it away, so you do not need `< /dev/tty`. On macOS that
alias is not enough on its own (its event reader cannot watch it), so sshelf resolves the real
device for itself.

It borrows fd 1 while the list is up. Placing the inline viewport means asking the terminal
where the cursor is, and that question goes out on stdout, which here is your pipe. So pick
points fd 1 at the terminal for as long as the list is drawn and hands the pipe back before it
prints the name. Nothing else writes to stdout in between.

Background for all three: [Decision log](decisions.md), D-039.
