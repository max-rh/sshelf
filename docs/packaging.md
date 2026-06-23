# Packaging & distribution

How `sshelf` ships to **Homebrew** (macOS + Linux), **Debian/Ubuntu** (`.deb`), **RedHat/Fedora**
(`.rpm`), and **crates.io**, for **x86_64 *and* arm64**. Releases are driven from GitHub Actions on
a `vX.Y.Z` tag.

**Chosen stack:**
- **[`dist`](https://opensource.axo.dev/cargo-dist/) (cargo-dist)** builds the binaries for all
  targets, makes the GitHub Release (tarballs + checksums), generates the **Homebrew formula**
  and pushes it to your tap, and emits a curl-able shell installer.
- Hand-written **companion workflows** attach what dist doesn't build to the same Release, each
  triggered via `workflow_run` *after* dist's "Release" completes (so they never race to create
  it): `release-deb.yml` (`.deb`, via `cargo deb`), `release-rpm.yml` (`.rpm`, via
  `cargo generate-rpm`, built as a **static musl** binary), and `release-crates.yml`
  (`cargo publish` to crates.io).
- **clap** generates shell completions + a man page (via `sshelf completions` / `sshelf man`).
- **crates.io:** cargo-dist has no built-in crates.io publish job, so `release-crates.yml` runs
  `cargo publish` (needs a `CARGO_REGISTRY_TOKEN` repo secret; skips cleanly if it's unset).

GitHub user is **`max-rh`**; the repo is `github.com/max-rh/sshelf`; the Homebrew tap is
`max-rh/homebrew-tap`.

**Contents**
1. [Prerequisites](#1-prerequisites)
2. [Target matrix (x86 + arm)](#2-target-matrix)
3. [Homebrew + tarballs via dist](#3-homebrew--tarballs-via-dist)
4. [Debian/Ubuntu `.deb`](#4-debianubuntu-deb)
5. [Shell completions & man page (clap)](#5-shell-completions--man-page)
6. [macOS code signing & notarization](#6-macos-code-signing--notarization)
7. [Cross-compilation reference](#7-cross-compilation-reference)
8. [Release checklist](#8-release-checklist)
9. [Appendix: manual Homebrew formula & APT repo](#9-appendix)

---

## 1. Prerequisites

Set in `Cargo.toml` before the first release:

```toml
[package]
# ...existing fields...
repository = "https://github.com/max-rh/sshelf"
homepage   = "https://max-rh.github.io/sshelf"   # the GitHub Pages docs site
readme     = "README.md"
# `exclude` keeps the published crate lean (drops docs/, .github/, examples/, the gif, etc.).
# `authors` is optional (cargo no longer auto-fills it) — omit it, or use a project ALIAS,
# never your personal email: this file is public on GitHub and copied into every .deb.
```

> **Email/privacy:** the `maintainer` in `[package.metadata.deb]` is shipped in every `.deb` and
> the `repository` link already gives users a way to reach you (Issues). Use a dedicated alias
> (e.g. a Gmail `+tag`, a forwarding address, or `sshelf@yourdomain`), **not** your private inbox.

Already in place in this repo:
- `[package.metadata.deb]` (for `cargo deb`) and the `.github/workflows/release-deb.yml` workflow.
- `sshelf completions <shell>` and `sshelf man` subcommands (clap).
- Dual `MIT OR Apache-2.0` license, committed `Cargo.lock`, MSRV `1.88`.

Conventions / facts that matter:
- A release is a git tag **`vX.Y.Z`** whose number **matches `Cargo.toml`'s `version`**.
- Ship **prebuilt binaries** (Debian/Ubuntu's packaged `rustc` often predates our MSRV 1.88).
- sshelf **`exec`s `ssh`** → the `.deb` depends on **`openssh-client`**; macOS has `ssh` built in.
- Linux secrets use a **pure-Rust** Secret Service client — **no `libdbus`/OpenSSL/`tokio` C
  build deps** — so cross-compiling is easy and the `.deb` needs no `-dev` packages. The Secret
  Service *daemon* is a `Recommends` (the `age`-vault fallback exists).

---

## 2. Target matrix

| OS / arch | Rust target | Built by |
|---|---|---|
| macOS Apple Silicon | `aarch64-apple-darwin` | dist, on an arm64 macOS runner |
| macOS Intel | `x86_64-apple-darwin` | dist (cross on the arm64 runner) |
| Linux x86_64 (Debian/Ubuntu amd64) | `x86_64-unknown-linux-gnu` | dist + `.deb` on `ubuntu-22.04` |
| Linux arm64 (Debian/Ubuntu arm64) | `aarch64-unknown-linux-gnu` | dist + `.deb` on `ubuntu-24.04-arm` |
| Linux x86_64/arm64 static (the `.rpm`) | `*-unknown-linux-musl` | `release-rpm.yml` (`cargo generate-rpm`) |

- **GitHub's free arm64 Linux runners** (`ubuntu-24.04-arm`, GA for *public* repos since Aug
  2025) build arm64 **natively** — no QEMU. (They aren't available to private repos on the free tier.)
- macOS runners: `macos-14`/`macos-15` are arm64, `macos-13` is the last Intel one. dist
  cross-compiles `x86_64-apple-darwin` on an arm64 runner (both SDKs are present).
- `*-gnu` is correct for `.deb`; `*-musl` gives a fully static tarball that runs on any distro
  (nice for the generic download and Homebrew-on-Linux), but isn't used for `.deb`.

---

## 3. Homebrew + tarballs via dist

### One-time setup

```sh
cargo install cargo-dist --locked      # installs the `dist` binary
dist init                              # interactive; safe to rerun anytime
```

Answer `dist init` with:
- **CI:** GitHub.
- **Installers:** `shell` and `homebrew`.
- **Targets:** `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`,
  `aarch64-unknown-linux-gnu` (add the two `*-musl` targets if you want static tarballs).
  **Decline Windows** — sshelf is Unix-only; remove `x86_64-pc-windows-msvc` if it's added.
- **Updater:** **no** (`install-updater = false`) — Homebrew/apt self-update, and the
  shell-installer audience is small.
- **Homebrew tap:** `max-rh/homebrew-tap`. Create that repo first and **initialize it with a
  README** (so it has a default branch dist can push the formula to). Then add a
  **`HOMEBREW_TAP_TOKEN`** secret to the `sshelf` repo — a PAT with write access to the tap repo,
  because the default `GITHUB_TOKEN` can't push to *another* repo. Without it the
  `publish-homebrew-formula` job fails.

`dist init` writes its config to **`dist-workspace.toml`** and **generates
`.github/workflows/release.yml`**. Let `dist init`/`dist generate` manage it (it pins
`cargo-dist-version` to your installed version). Our config:

```toml
[workspace]
members = ["cargo:."]

[dist]
cargo-dist-version = "0.32.0"    # managed by dist; don't hand-edit
ci = "github"
installers = ["shell", "homebrew"]
tap = "max-rh/homebrew-tap"
targets = [
  "aarch64-apple-darwin", "x86_64-apple-darwin",
  "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
]
publish-jobs = ["homebrew"]
install-path = "CARGO_HOME"
install-updater = false
```

> **Drop the Windows target:** `dist init` adds `x86_64-pc-windows-msvc` by default. sshelf is
> Unix-only (the connect path uses `exec()`), so the Windows build can't compile — remove that
> target from `targets`, leaving the four above.

### Releasing

```sh
# bump version in Cargo.toml, commit, then:
git tag v0.1.0
git push origin v0.1.0
```

The tag triggers `release.yml` (dist): it builds every target, creates the GitHub Release
(tarballs + `dist-manifest.json` + shell installer), and **updates the formula in
`max-rh/homebrew-tap`**. When that workflow *finishes*, `release-deb.yml` runs via `workflow_run`
and attaches the `.deb`s to the Release (§4) — sequenced, not racing.

Users then:

```sh
brew install max-rh/tap/sshelf        # macOS or Linux, picks the right arch automatically
# or the shell installer dist prints in the release notes:
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/max-rh/sshelf/releases/latest/download/sshelf-installer.sh | sh
```

> **macOS signing:** Developer ID signing + notarization need the **paid** Apple Developer
> Program and are **optional** — a CLI installed via Homebrew runs fine unsigned. Do add a free
> **ad-hoc** `codesign` step for a stable signature. See §6 for the full free-vs-paid breakdown.

> **Completions in Homebrew:** dist's generated formula installs the binary. Shell completions
> are available immediately via `sshelf completions <shell>` (§5); the `.deb` installs them
> system-wide. If you want Homebrew to *install* them too, use the manual formula in the
> [appendix](#9-appendix) with `generate_completions_from_executable` instead of dist's formula.

---

## 4. Debian/Ubuntu `.deb`

dist doesn't build Debian packages, so a companion workflow does. The package metadata is
already in `Cargo.toml`:

```toml
[package.metadata.deb]
maintainer = "max-rh <max-rh@mail.com>"    # public alias, not a personal inbox
depends = "$auto, openssh-client"          # we exec ssh
recommends = "gnome-keyring"               # Secret Service daemon (vault is the fallback)
section = "utils"
# ...assets: the binary, README/SECURITY, and the generated completions + man page...
```

The workflow [`.github/workflows/release-deb.yml`](../.github/workflows/release-deb.yml) builds
**natively** on each arch (`ubuntu-22.04` for amd64, `ubuntu-24.04-arm` for arm64), generates
completions + the man page with the `sshelf` subcommands, runs `cargo deb --no-build`, and
attaches `target/debian/*.deb` to the Release. It triggers on **`workflow_run`** — i.e. *after*
dist's `release.yml` completes — so the two never race to create the Release: dist owns creation,
this only attaches. (A `release: published` trigger wouldn't fire, because dist creates the
Release with `GITHUB_TOKEN`, and `GITHUB_TOKEN`-created events can't trigger downstream workflows.)
Outline:

```yaml
on:
  workflow_run: { workflows: ["Release"], types: [completed] }
jobs:
  deb:
    # only after a successful, tag-triggered Release run; head_branch is the tag
    if: github.event.workflow_run.conclusion == 'success' &&
        github.event.workflow_run.event == 'push' &&
        startsWith(github.event.workflow_run.head_branch, 'v')
    strategy:
      matrix:
        include:
          - { arch: amd64, os: ubuntu-22.04 }
          - { arch: arm64, os: ubuntu-24.04-arm }
    steps:
      - uses: actions/checkout@v4
        with: { ref: "${{ github.event.workflow_run.head_branch }}" }   # the release tag
      - run: cargo build --release --locked
      - run: |                       # generate the packaged extras
          bin=target/release/sshelf
          "$bin" completions bash > dist-extra/sshelf.bash   # + zsh, fish
          "$bin" man              > dist-extra/sshelf.1
      - run: cargo deb --no-build    # .deb arch == runner arch
      - uses: softprops/action-gh-release@v2
        with:
          tag_name: "${{ github.event.workflow_run.head_branch }}"
          files: target/debian/*.deb
```

Users install a downloaded package with:

```sh
sudo apt install ./sshelf_0.1.0-1_amd64.deb     # resolves deps (openssh-client, …)
```

For a true `apt install sshelf` (no file download), host a **signed APT repo** — see the
[appendix](#9-appendix). That's the most involved channel; the `.deb`-on-Releases above covers
most users.

---

## 4b. RedHat/Fedora `.rpm` (static musl)

Same shape as the `.deb`, with two differences. The package metadata is in `Cargo.toml`
(`[package.metadata.generate-rpm]`, built by [`cargo-generate-rpm`](https://github.com/cat-in-136/cargo-generate-rpm)),
and the binary is built **static musl** (`*-unknown-linux-musl`) so one `.rpm` runs on any RPM
distro — Fedora, RHEL/Rocky/Alma, openSUSE — **regardless of glibc version**. (sshelf is
distro-agnostic at runtime: it only shells out to the system `ssh`/`sftp`/`ps`/`kill`, all OpenSSH/
procps, and the Linux keyring is the pure-Rust Secret Service with the `age`-vault fallback — none
of it is Debian- or RPM-specific.) `auto-req = "no"` stops rpm from adding bogus shared-lib
`Requires` to a static binary; we declare `openssh-clients` explicitly.

[`.github/workflows/release-rpm.yml`](../.github/workflows/release-rpm.yml) mirrors the `.deb`
workflow: `workflow_run` after dist's Release, a matrix of `x86_64` (`ubuntu-22.04`) and `aarch64`
(`ubuntu-24.04-arm`, native), `rustup target add <musl>`, `cargo build --target <musl>`, generate
completions/man, then `cargo generate-rpm --target <musl>` (which rewrites the `target/release`
asset paths to the per-target dir) and attaches the `.rpm`. Users install with:

```sh
sudo dnf install ./sshelf-0.8.0-1.x86_64.rpm     # or .aarch64.rpm
```

> **Why musl, not glibc like the `.deb`:** a glibc binary built on the CI runner only runs on
> distros with an equal-or-newer glibc, which excludes older RHEL. Static musl sidesteps that
> entirely. (The `.deb` keeps glibc since Debian/Ubuntu users build/run on a known-recent glibc.)

---

## 4c. crates.io (`cargo install sshelf`)

cargo-dist has **no built-in crates.io publish job** (`publish-jobs` only knows `homebrew`/`npm`/
custom `./jobs`), so publishing is a separate companion workflow,
[`release-crates.yml`](../.github/workflows/release-crates.yml): `workflow_run` after the Release,
then `cargo publish --locked`. It needs a **`CARGO_REGISTRY_TOKEN`** repo secret (a crates.io API
token, ideally scoped to the `sshelf` crate); the step skips cleanly if the secret is unset, so the
workflow stays green before it's added. The crate's `Cargo.toml` carries the required metadata
(`description`, `license`, `keywords`, `categories`, `repository`, `homepage`) and an `exclude`
that drops `docs/`/`.github/`/`examples/` from the published tarball. `cargo publish` builds from
the tag's source, so it's independent of the release binaries.

```sh
cargo install sshelf      # once published
```

---

## 5. Shell completions & man page

`sshelf` generates these itself (clap), so packaging needs no extra tooling:

```sh
sshelf completions bash      # also: zsh, fish, elvish, powershell
sshelf man                   # roff man page on stdout
```

- **`.deb`** ships them system-wide (`/usr/share/bash-completion/...`, `/usr/share/man/man1/...`)
  — generated in the deb workflow (§4) and listed in `[package.metadata.deb].assets`.
- **Homebrew** (dist formula): users can `source <(sshelf completions zsh)`, or switch to the
  manual formula ([appendix](#9-appendix)) which auto-installs via
  `generate_completions_from_executable(bin/"sshelf", "completions")` and `man1.install`.
- **Tarball users:** the binary is self-sufficient — run the subcommands as needed.

Implementation: `src/main.rs` builds the `clap::Command` with `Cli::command()` and feeds it to
`clap_complete::generate(...)` / `clap_mangen::Man::new(...)`. No `build.rs` needed.

---

## 6. macOS signing — and why you don't need the $99 Apple program

**Developer ID signing + notarization require the paid Apple Developer Program ($99/yr).** You
do **not** need it to ship `sshelf`, because it's a **CLI distributed via Homebrew**, not a GUI
app. Here's the free path and exactly what (if anything) you give up.

**What macOS actually enforces:**
- **Apple Silicon refuses to run a binary with *no* signature** — but a free **ad-hoc** signature
  satisfies it, and the macOS toolchain applies one automatically when it links the binary.
  (Intel Macs don't even require that.)
- **Gatekeeper's "unidentified developer" block** only hits files carrying the
  `com.apple.quarantine` xattr, which **browsers** set on download. `curl`, `git`, and
  **Homebrew don't set it** for CLI **formulae** — so a `brew install`-ed binary runs with no
  Gatekeeper prompt, signed or not. (Homebrew's recent tightening — deprecating
  `--no-quarantine`, disabling failing **casks** in Sept 2026 — targets **GUI `.app` casks**,
  not CLI formulae like sshelf.)

**Free distribution that "just works" (recommended order):**
1. **Homebrew** — `brew install max-rh/tap/sshelf`. No quarantine, no Gatekeeper prompt, no Apple
   account. This is how most open-source Rust CLIs ship. ✅ your main path.
2. **Build from source** — `cargo install --git https://github.com/max-rh/sshelf`, or a formula
   with `depends_on "rust" => :build`. Compiled locally → no signing questions at all.
3. **Ad-hoc sign in CI (free, recommended) — the chosen hardening.** Guarantees a *stable*
   signature on every macOS artifact, no cert/account/Apple-program. Verified on an Intel build:
   the default `cargo build` leaves the binary **"not signed at all"**; one command fixes it:
   ```sh
   codesign --sign - --force target/<triple>/release/sshelf          # ad-hoc, free
   codesign -dvv target/<triple>/release/sshelf 2>&1 | grep Signature  # -> Signature=adhoc
   ```
   (arm64 binaries are auto ad-hoc-signed by the linker — Apple Silicon requires it to run; this
   step also covers the cross-built **x86_64** and settles the Keychain point below.)

   **Wiring it into dist:** dist builds macOS on macOS runners and generates
   `.github/workflows/release.yml`. Add this step to that file's macOS build job, right after the
   build, so both arches get a stable ad-hoc identity:
   ```yaml
   - name: Ad-hoc sign macOS binaries (free, stable identity)
     if: runner.os == 'macOS'
     run: find target -type f -name sshelf -perm +111 -exec codesign --sign - --force {} \;
   ```
   Re-apply it if you re-run `dist init` (which regenerates `release.yml`). The Linux/`.deb` side
   needs no signing.

**The one thing you lose without paying:** a user who **downloads the release `.tar.gz` directly
in a browser** gets it quarantined, so Gatekeeper blocks it until they clear it once:

```sh
xattr -dr com.apple.quarantine "$(command -v sshelf)"     # or: right-click the file → Open
```

Document that, or just steer direct-download users to Homebrew. **Notarization is the only thing
that removes this** for direct downloads — and that needs the paid program.

**Keychain prompt (sshelf-specific):** the per-connect Keychain prompt happens when the signature
is *unstable* (re-built dev binaries) or *absent*. A released, **ad-hoc-signed** binary has a
stable identity, and sshelf's askpass child is the **same binary file** as the parent, so the
Keychain ACL one creates is honored by the other → no prompt. If a user still hits keychain
friction, the **`age` vault** (`SSHELF_VAULT_PASSPHRASE`) bypasses the OS keychain entirely — the
guaranteed-free fallback (see `docs/security.md`).

**If you ever do pay** ($99/yr) for friction-free direct downloads: sign with a Developer ID
Application cert under the hardened runtime, then notarize (dist can automate this from CI secrets):

```sh
codesign --force --options runtime --timestamp \
  --sign "Developer ID Application: NAME (TEAMID)" target/<triple>/release/sshelf
ditto -c -k --keepParent target/<triple>/release/sshelf sshelf.zip
xcrun notarytool submit sshelf.zip --key AuthKey.p8 --key-id KEYID --issuer ISSUER_UUID --wait
```

(You can't `stapler staple` a bare binary/zip — only `.app`/`.dmg`/`.pkg` — but a notarized zip
is fine for Homebrew; ship a stapled `.pkg` for offline direct downloads.)

---

## 7. Cross-compilation reference

- **Native (what dist + the deb workflow use):** build each target on a runner of that arch.
- **`cross`** ([cross-rs/cross](https://github.com/cross-rs/cross)): `cross build --release
  --target aarch64-unknown-linux-gnu` (Docker-based; handles the linker/sysroot).
- **Plain cargo cross-link (Linux):**
  ```sh
  sudo apt-get install -y gcc-aarch64-linux-gnu
  rustup target add aarch64-unknown-linux-gnu
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    cargo build --release --target aarch64-unknown-linux-gnu
  ```
  (Pure-Rust apart from libc, so no other `-dev` libs are needed.)
- **macOS universal binary:** build both Darwin targets, then
  `lipo -create -output sshelf <arm64> <x86_64>`.

---

## 8. Release checklist

1. Bump `version` in `Cargo.toml`; update a `CHANGELOG.md`.
2. CI green (`cargo test`, `clippy -D warnings`, `cargo fmt --check`).
3. `git tag vX.Y.Z && git push origin vX.Y.Z`.
4. Watch `release.yml` (dist → tarballs + Homebrew tap + shell installer); when it finishes,
   `release-deb.yml` runs via `workflow_run` and attaches the `.deb`s. macOS artifacts are ad-hoc
   signed (§6).
5. Smoke-test one install per channel and **connect to a host from inside the TUI** (the
   real-TTY acceptance check in `docs/progress.md`):
   - `brew install max-rh/tap/sshelf`
   - `sudo apt install ./sshelf_*_amd64.deb`
   - `sudo dnf install ./sshelf-*.x86_64.rpm`
   - `cargo install sshelf` (after the crates.io publish lands)

---

## 9. Appendix

### Manual Homebrew formula (alternative to dist's)

Use this if you want Homebrew to install completions + the man page, or prefer not to run dist.
Put it in `max-rh/homebrew-tap` as `Formula/sshelf.rb`:

```ruby
class Sshelf < Formula
  desc "TUI for managing and connecting to SSH hosts"
  homepage "https://github.com/max-rh/sshelf"
  version "0.1.0"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm   { url "https://github.com/max-rh/sshelf/releases/download/v#{version}/sshelf-aarch64-apple-darwin.tar.gz"; sha256 "…" }
    on_intel { url "https://github.com/max-rh/sshelf/releases/download/v#{version}/sshelf-x86_64-apple-darwin.tar.gz";  sha256 "…" }
  end
  on_linux do
    on_arm   { url "https://github.com/max-rh/sshelf/releases/download/v#{version}/sshelf-aarch64-unknown-linux-gnu.tar.gz"; sha256 "…" }
    on_intel { url "https://github.com/max-rh/sshelf/releases/download/v#{version}/sshelf-x86_64-unknown-linux-gnu.tar.gz";  sha256 "…" }
  end

  def install
    bin.install "sshelf"
    generate_completions_from_executable(bin/"sshelf", "completions")
    (man1/"sshelf.1").write Utils.safe_popen_read(bin/"sshelf", "man")
  end

  test do
    assert_match "sshelf", shell_output("#{bin}/sshelf --version")
  end
end
```

(`brew bump-formula-pr` can update the version + SHAs on each release.)

### Signed APT repository (`apt install sshelf`)

Host a GPG-signed repo (e.g. on GitHub Pages) with `reprepro`:

```sh
gpg --full-generate-key
gpg --armor --export YOUR_KEY_ID > sshelf-archive-keyring.asc
# apt/conf/distributions: Codename: stable / Architectures: amd64 arm64 / Components: main / SignWith: YOUR_KEY_ID
reprepro -b apt includedeb stable sshelf_0.1.0-1_amd64.deb sshelf_0.1.0-1_arm64.deb
# publish the ./apt tree (dists/, pool/, signed Release/InRelease) to Pages
```

Users:

```sh
curl -fsSL https://max-rh.github.io/sshelf-apt/sshelf-archive-keyring.asc \
  | sudo tee /usr/share/keyrings/sshelf.asc >/dev/null
echo "deb [signed-by=/usr/share/keyrings/sshelf.asc] https://max-rh.github.io/sshelf-apt stable main" \
  | sudo tee /etc/apt/sources.list.d/sshelf.list
sudo apt update && sudo apt install sshelf
```

An Ubuntu **PPA** (Launchpad) is the native alternative but requires vendoring crates
(`cargo vendor`, `dh-cargo`) because Launchpad builders have no network — more work than the
signed-repo-of-prebuilt-`.deb`s above for the same `apt install` UX.

---

### Sources
- dist (cargo-dist): <https://opensource.axo.dev/cargo-dist/>
- GitHub arm64 Linux runners GA (public repos): <https://github.blog/changelog/2025-08-07-arm64-hosted-runners-for-public-repositories-are-now-generally-available/>
- `cargo-deb`: <https://github.com/kornelski/cargo-deb>
- Apple notarization: <https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution>
- Homebrew Formula Cookbook: <https://docs.brew.sh/Formula-Cookbook>
