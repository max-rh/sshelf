# Transferring files

`Ctrl-t` on a host opens a **dual-pane transfer screen**: your local files on one side, the
host's on the other. Mark what you want and send it in either direction over SFTP, with fuzzy
search on both sides, live progress, and a `F7` to create directories without leaving.

sshelf authenticates **once**: it opens an `ssh` ControlMaster that reuses the host's normal
auth (keys/agent/ProxyJump, or the stored password, supplied the same way as on connect) and
runs `sftp` over it. No per-file re-prompts, and `~/.ssh/config` is never touched. Remote
listing and transfers run on a background thread, so the UI stays responsive on slow links.

## Keys

| Key | Action |
|---|---|
| _type_ | filter the focused pane |
| `Tab` | switch the focused pane (local ↔ remote) |
| `↑` / `↓`, `Ctrl-p` / `Ctrl-n` | move the selection |
| `Space` | **mark / unmark** the selected file or folder |
| `Ctrl-a` | mark everything the filter shows; press again to clear every mark |
| `Ctrl-s` | **send** the marked entries (or, with none marked, the selected one) into the other pane's directory |
| `F7` / `Ctrl-f` | **create a directory** in the focused pane |
| `→` / `Enter` | open the selected directory (on a file: send it) |
| `←` | go up a directory |
| `Backspace` | edit the filter, or go up when it's empty |
| `Esc` | cancel a running transfer, else clear marks, else clear the filter, else close the screen |

## Marking and sending several at once

`Space` marks the entry under the cursor; marked rows get a `•` and the accent color, and the
pane title counts them. `Ctrl-s` then sends **all of them**, files and folders alike and folders
recursively, into the other pane's current directory, one at a time through the same single
authenticated connection. The progress line counts through the batch (`2 of 5  report.pdf →
deploy@host`).

- Marks are positional. Changing directory, refreshing a listing, or a listing error drops
  them; they are never remembered per path. `Esc` clears them explicitly.
- Sending consumes the marks; the queue becomes the record of what's going.
- An entry the destination already has is **skipped** and the queue carries on; the summary
  names what was passed over (`sent 3 of 4 · skipped dup.txt (already there)`). A real transfer
  **failure** stops the rest, since whatever broke will usually break the next one too, and the
  status says how many were left unsent.
- `Space` marks rather than typing a space into the filter. Filenames containing spaces still
  match by the rest of their name.

## Creating a directory (`F7`)

`F7` (or `Ctrl-f`, if your terminal keeps `F7` for itself) opens a one-line input at the bottom
of the focused pane. Type a name, `Enter` creates it **in that pane's current directory**,
`Esc` cancels. It works on both sides; the remote one goes through the same SFTP connection.

The name must be a single directory name: no `/` (this creates one directory, not a path), no
control characters, not `.` or `..`, and **not a name that already exists**. An existing
directory is never adopted, so the input stays open with an error and you can pick another
name. On success the listing refreshes and the new directory lands under the cursor.

## Hidden files

Both panes list hidden entries. The local pane always did; the remote one lists with
`ls -la` through `sftp`, so dotfiles and dot-directories show up on the server side too and
you can open `.config` or `.ssh` the same way you open any other directory. `.` and `..`
are never listed on either side (`←` goes up).

There is no show/hide toggle. If a directory has too much in it, type a `.` into the pane
filter: that keeps the names with a dot in them, and the hidden ones sort to the top.

## Behavior & limits

- Directories are shown as `name/` and symlinks as `name@`, and symlinks are skipped.
- A same-named file or folder already present in the destination is **skipped** (with a
  message), never overwritten. What that promise rests on differs by direction, so it is spelled
  out under [What "never overwritten" covers](#what-never-overwritten-covers) below.
- One transfer runs at a time: a batch is a queue, not parallel copies. Single-file downloads
  show bytes + percent; folders and uploads show as in-flight (cancelable with `Esc`, which
  abandons the rest of the queue too).
- Filenames are shell-quoted (spaces are fine) and control characters are stripped from
  display.
- A remote listing is given 60 seconds and a remote `mkdir` 30 seconds before sshelf kills the
  `sftp` running it, and the pane says `timed out after 60s listing <path>` rather than sitting
  there. A listing is also capped at 16 MiB of output and 50,000 entries; past that the pane
  says `listing truncated at 50000 entries` instead of passing a partial directory off as the
  whole of it. Closing the screen never waits on any of this: `Esc` gets the terminal back even
  if the server has stopped answering.
- The connection uses `StrictHostKeyChecking=accept-new`, like connect: a first-time host key
  is trusted on first use, a **changed** key still hard-fails. See [Security](security.md).
- Renaming, deleting, changing permissions, and overwriting are not in this version.

## What "never overwritten" covers

**Downloading a single file** never replaces anything. The bytes land on a private
`.sshelf-part-…` name in the destination directory first, and the finished file is put in place
with a link, which fails if any name is already there. A symlink counts as a name, and it is
never followed, so nothing can redirect the write. If the name turned up while the transfer was
running, the entry is skipped exactly as the pre-flight check would have skipped it, and the
queue carries on.

Some filesystems have no hard links at all: exFAT and FAT32, which is what a USB stick usually
is, and a fair number of SMB and FUSE mounts. Downloading onto one of those checks that the name
is still free and then moves the temporary onto it, which is a smaller window than writing the
final name directly but not the same guarantee. The bytes are never thrown away over it.

**Downloading a folder** and **uploading anything** are checked against the last listing of the
destination, and nothing more. There is no no-replace open to be had over the `sftp` command
line, and a folder cannot be installed with a link. The window is small (sshelf refreshes the
remote listing immediately before each send), but a file that appears on the server between that
refresh and the write can be overwritten. If that matters for what you are sending, look at the
destination first.

A remote directory past the 50,000-entry cap is the one case where that check cannot be made at
all, so sshelf refuses the send rather than guess:

```text
the destination listing is incomplete (cut at 50000 entries) — sshelf can't promise not to overwrite there
```

Downloads into such a directory are fine, since they do not rely on the listing.

## Where the connection lives

The screen holds one `ssh` ControlMaster, and its control socket lives in a directory sshelf
creates for that session with mode 0700: `$XDG_RUNTIME_DIR/sshelf/mux-<ulid>/m.sock` when
`XDG_RUNTIME_DIR` is set, otherwise `~/.local/share/sshelf/run/mux-<ulid>/m.sock`. It used to be
a predictable name straight in `/tmp`, where another account on the machine could take the path
first. Both the socket and the directory are removed when the screen closes. A stray `mux-*`
directory from a crash is harmless and can be deleted.

## Debugging a failing transfer

The status line shows the underlying `sftp` error. For the full story:

```sh
sshelf --transfer-log ~/.local/share/sshelf/transfer.log     # or $SSHELF_TRANSFER_LOG
```

This appends every `ssh` and `sftp` command, the local and remote paths they touch, their
stderr, and every value the host's `extra_args` contributes. **No password is logged**: a stored
secret reaches `ssh` via `SSH_ASKPASS` and never the command line. Taken together, though, that
is a full description of the connection, so keep the file somewhere private. sshelf creates it
mode 0600 and refuses to follow a symlink at that path (you get one line on stderr and no log),
which is why the example points inside the data directory rather than at `/tmp`.
