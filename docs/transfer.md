# Transferring files

`Ctrl-t` on a host opens a **dual-pane transfer screen**: your local files on one side, the
host's on the other. Copy files or whole folders in either direction over SFTP, with fuzzy
search on both sides and live progress.

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
| `→` / `Enter` | open the selected directory (on a file: send it) |
| `Ctrl-s` | **send** the selected file or folder (recursive) into the other pane's directory |
| `←` | go up a directory |
| `Backspace` | edit the filter, or go up when it's empty |
| `Esc` | cancel a running transfer, else clear the filter, else close the screen |

## Behavior & limits

- Directories are shown as `name/` and symlinks as `name@` — **symlinks are skipped**.
- A same-named file or folder already present in the destination is **skipped** (with a
  message), never overwritten.
- One transfer runs at a time. Single-file downloads show bytes + percent; folders and
  uploads show as in-flight (cancelable with `Esc`).
- Filenames are shell-quoted (spaces are fine) and control characters are stripped from
  display.
- The connection uses `StrictHostKeyChecking=accept-new`, like connect: a first-time host key
  is trusted on first use, a **changed** key still hard-fails. See [Security](security.md).

## Debugging a failing transfer

The status line shows the underlying `sftp` error. For the full story:

```sh
sshelf --transfer-log /tmp/sshelf-transfer.log     # or $SSHELF_TRANSFER_LOG
```

This appends every `ssh`/`sftp` command and its stderr to the file. **No secrets are
logged** — passwords reach `ssh` via `SSH_ASKPASS`, never the command line.
