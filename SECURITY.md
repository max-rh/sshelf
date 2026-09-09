# Security Policy

## Reporting a vulnerability

Please report security issues **privately** rather than filing a public issue:

- Preferred: open a private advisory at [GitHub → Security → Report a vulnerability](https://github.com/max-rh/sshelf/security/advisories/new).
- Email: max-rh@mail.com

Reports are acknowledged and fixed before public disclosure.

## Threat model (summary)

For the other half of the picture (what sshelf reads, writes, runs, and sends on your machine
in normal use), see [`PRIVACY.md`](PRIVACY.md).

`sshelf` can store SSH passwords so it can auto-supply them. **Prefer SSH keys / an agent
wherever possible.** Stored passwords are the least secure option offered.

**Where secrets live:** the OS keyring (macOS Keychain, Linux Secret Service via a pure-Rust
client) by default; or, if `SSHELF_VAULT_PASSPHRASE` is set, an `age`-encrypted `vault.age`
(scrypt + ChaCha20-Poly1305) for headless/automation use. Secrets are keyed by host id and are
**never** written to `hosts.toml`, logs, shell history, or process arguments.

**Vault mode and the environment:** in vault mode the askpass helper runs as a child of `ssh`
and reads `SSHELF_VAULT_PASSPHRASE` from the environment to decrypt the vault, so for
password/passphrase hosts that env var is necessarily visible to the `ssh` process tree (e.g.
in `/proc/<pid>/environ`, readable by your own user). For hosts with **no** stored secret,
sshelf strips the variable from the environment before exec'ing `ssh`. This is within the
threat model below (your own user on a machine you control), but treat the vault passphrase
accordingly on shared systems, or prefer the OS keyring, which needs no env var.

**How the secret reaches ssh:** via `SSH_ASKPASS`. `sshelf` is re-invoked by `ssh` and prints
the secret on stdout. Because the text of a keyboard-interactive prompt is written by the
server, the helper is told which secret it is holding and answers only that one. A password
host answers a login-password prompt; a key host answers only OpenSSH's own
`Enter passphrase for key '<path>':`, and only when `<path>` is one of the identity files that
connect passed with `-i`. A secret-shaped prompt of the wrong kind is declined, and is never
answered with a queued verification code instead. Key hosts also connect with
`-o PreferredAuthentications=publickey` (plus `keyboard-interactive` for 2FA hosts), so a
server cannot steer a key host into a password prompt at all.
(`-o StrictHostKeyChecking=accept-new` keeps the first-connect host-key prompt out of the
helper while still verifying known hosts.)

**Jump hosts never see the helper.** A `ProxyJump` hop is a child of `ssh` and inherits the
same environment, and OpenSSH does not pass the destination's `-o` options down to it. So with
one jump host and something to protect, sshelf runs the hop through an explicit `ProxyCommand`
with `BatchMode=yes`, `PasswordAuthentication=no` and `KbdInteractiveAuthentication=no`, which
leaves it an agent or an unencrypted key file and nothing else. With two or more hops nothing
is wired at all and `ssh` asks for the target's secret on the terminal.

### Protected against
- Plaintext-on-disk exposure (secrets are in the keyring or encrypted at rest).
- Process-listing / argv leakage (no `sshpass -p`).
- Shell-history leakage; `hosts.toml` is safe to share/commit (no secrets).
- Config corruption (atomic writes).

### NOT protected against (out of scope)
- A root/admin attacker or malware on your machine (can read process memory / the keyring).
- Keyloggers (can capture a typed vault passphrase).
- A compromised OS keyring daemon.
- Physical theft without full-disk encryption.
- Unencrypted backups/cloud-sync of `vault.age` (it's encrypted, but treat it as sensitive).

Assumption: `sshelf` runs on a machine you control and trust. There is **no recovery** if you
forget the vault passphrase.

## Fixed in 0.14.0

An outside review read the source, the configuration, the history, the dependencies and the CI,
and found thirteen things worth fixing. All of them are closed in 0.14.0:

- The askpass helper could hand a key passphrase to a server that asked for a password.
- A jump host inherited the askpass helper, so a compromised bastion could ask for the target's
  secret.
- The version input to the release workflow could reach a shell before it was validated.
- The transfer screen's control socket sat at a predictable path in `/tmp`.
- Release jobs used mutable action tags, piped an installer from the network, and held write
  permission in jobs that only needed to read.
- The `.deb`, `.rpm` and crates.io jobs rebuilt from a tag name rather than the commit the
  release run built.
- A remote SFTP peer could keep the transfer screen from closing.
- The lockfile carried unsound dependency versions with fixes available.
- Plain CLI output printed host fields, including terminal control characters, unfiltered.
- The no-overwrite check for transfers was a check against the last listing, not an atomic one.
- Temporary files and diagnostic logs used predictable names and followed symlinks.
- A custom `--config` path changed the permissions of its existing parent directory.
- The 2FA code was visible while it was typed.

## Platform notes
- macOS, unsigned builds: the re-invoked askpass helper reads Keychain as a *separate*
  process; because Keychain ACLs are tied to code signature, an unsigned dev build may prompt
  for Keychain access on each connect. Use a signed release build, or the vault, to avoid this.
- Windows is not supported in v1.
