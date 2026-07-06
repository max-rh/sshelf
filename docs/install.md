# Install

sshelf runs on **macOS and Linux**, x86_64 and arm64. The prebuilt packages need **no Rust
toolchain**; at runtime sshelf wants **OpenSSH 8.4+** on your machine (password auto-supply
rides on `SSH_ASKPASS_REQUIRE`, added in OpenSSH 8.4 — see the [FAQ](faq.md) if unsure).

## Homebrew (macOS or Linux)

```sh
brew install max-rh/tap/sshelf
```

## Shell installer

Downloads the prebuilt binary for your platform:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/max-rh/sshelf/releases/latest/download/sshelf-installer.sh | sh
```

## Debian / Ubuntu (`.deb`)

Grab the `.deb` for your architecture from the
[latest release](https://github.com/max-rh/sshelf/releases/latest), then:

```sh
sudo apt install ./sshelf_*_amd64.deb      # or *_arm64.deb
```

## Fedora / RHEL / Rocky / openSUSE (`.rpm`)

The `.rpm` is a static build, so one package runs on any RPM distro regardless of glibc.
Grab it from the [latest release](https://github.com/max-rh/sshelf/releases/latest), then:

```sh
sudo dnf install ./sshelf-*.x86_64.rpm     # or .aarch64.rpm
```

## Gentoo

Via the community-maintained Masterwolf overlay (unofficial; thanks to
[@masterwolf-git](https://github.com/masterwolf-git)):

```sh
eselect repository enable masterwolf
emerge --sync
emerge --ask app-admin/sshelf
```

## Cargo (crates.io)

Needs **Rust 1.88+**:

```sh
cargo install sshelf
```

## After installing

- **Shell tab-completion** (subcommands + flags) ships with every package — open a **new
  shell** (or `exec $SHELL`) so it loads. Completion of your saved **host names** takes one
  more line in your shell rc: see [Shell completions](cli.md#shell-completions).
- On Linux, secrets use the **Secret Service** (GNOME Keyring / KWallet) through a pure-Rust
  backend — no `libdbus` or OpenSSL packages needed. On a headless box with no keyring, see
  [the age vault](passwords-2fa.md#where-secrets-live).
- Next stop: the [Quickstart](quickstart.md).
