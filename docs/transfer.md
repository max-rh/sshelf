# Transferring files

`Ctrl-t` on a host opens a **dual-pane transfer screen**: your local files on one side, the
host's on the other. Mark what you want and send it in either direction over SFTP, with fuzzy
search on both sides, live progress, and a `F7` to create directories without leaving.

sshelf authenticates **once**: it opens an `ssh` ControlMaster that reuses the host's normal
auth (keys/agent/ProxyJump — or the stored password, supplied the same way as on connect) and
runs `sftp` over it. No per-file re-prompts, and `~/.ssh/config` is never touched. Remote
listing and transfers run on a background thread, so the UI stays responsive on slow links.

## Keys

| Key | Action |
|---|---|
| _type_ | filter the focused pane |
| `Tab` | switch the focused pane (local ↔ remote) |
| `↑` / `↓`, `Ctrl-p` / `Ctrl-n` | move the selection |
| `Space` | **mark / unmark** the selected file or folder |
| `Ctrl-a` | mark everything the filter shows — press again to clear every mark |
| `Ctrl-s` | **send** the marked entries (or, with none marked, the selected one) into the other pane's directory |
| `F7` / `Ctrl-f` | **create a directory** in the focused pane |
| `→` / `Enter` | open the selected directory (on a file: send it) |
| `←` | go up a directory |
| `Backspace` | edit the filter, or go up when it's empty |
| `Esc` | cancel a running transfer, else clear marks, else clear the filter, else close the screen |

## Marking and sending several at once

`Space` marks the entry under the cursor; marked rows get a `•` and the accent color, and the
pane title counts them. `Ctrl-s` then sends **all of them** — files and folders alike, folders
recursively — into the other pane's current directory, one at a time through the same single
authenticated connection. The progress line counts through the batch (`2 of 5  report.pdf →
deploy@host`).

- **Marks are positional.** Changing directory, refreshing a listing, or a listing error drops
  them; they are never remembered per path. `Esc` clears them explicitly.
- **Sending consumes the marks** — the queue becomes the record of what's going.
- An entry the destination already has is **skipped** and the queue carries on; the summary
  names what was passed over (`sent 3 of 4 · skipped dup.txt (already there)`). A real transfer
  **failure** stops the rest, since whatever broke will usually break the next one too — the
  status says how many were left unsent.
- `Space` marks rather than typing a space into the filter. Filenames containing spaces still
  match by the rest of their name.

## Creating a directory (`F7`)

`F7` (or `Ctrl-f`, if your terminal keeps `F7` for itself) opens a one-line input at the bottom
of the focused pane. Type a name, `Enter` creates it **in that pane's current directory**,
`Esc` cancels. It works on both sides — the remote one goes through the same SFTP connection.

The name must be a single directory name: no `/` (this creates one directory, not a path), no
control characters, not `.` or `..`, and **not a name that already exists** — an existing
directory is never adopted, so the input stays open with an error and you can pick another
name. On success the listing refreshes and the new directory lands under the cursor.

## Behavior & limits

- Directories are shown as `name/` and symlinks as `name@` — **symlinks are skipped**.
- A same-named file or folder already present in the destination is **skipped** (with a
  message), never overwritten.
- One transfer runs at a time — a batch is a queue, not parallel copies. Single-file downloads
  show bytes + percent; folders and uploads show as in-flight (cancelable with `Esc`, which
  abandons the rest of the queue too).
- Filenames are shell-quoted (spaces are fine) and control characters are stripped from
  display.
- The connection uses `StrictHostKeyChecking=accept-new`, like connect: a first-time host key
  is trusted on first use, a **changed** key still hard-fails. See [Security](security.md).
- Renaming, deleting, changing permissions, and overwriting are not in this version.

## Debugging a failing transfer

The status line shows the underlying `sftp` error. For the full story:

```sh
sshelf --transfer-log /tmp/sshelf-transfer.log     # or $SSHELF_TRANSFER_LOG
```

This appends every `ssh`/`sftp` command and its stderr to the file. **No secrets are
logged** — passwords reach `ssh` via `SSH_ASKPASS`, never the command line.
