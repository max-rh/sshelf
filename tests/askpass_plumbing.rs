//! End-to-end plumbing for the askpass helper, without a server.
//!
//! The unit tests in `src/askpass.rs` cover the classifier as a pure function. This covers the
//! wiring around it: a real `sshelf` process builds the argv and the environment for a real
//! connect, `exec()`s a stub `ssh` that stands in for OpenSSH, and that stub calls back into
//! `$SSH_ASKPASS` with the prompts a hostile endpoint would send. What comes back on stdout is
//! the thing finding H-02 was about.
//!
//! The stub records, per prompt, the helper's stdout and exit code, plus the argv it was given,
//! so one run proves the classifier, `configure_askpass`, `build_args` and `jump_plan` all agree.
//! Secrets go through the headless `age` vault (`SSHELF_VAULT_PASSPHRASE`), so no keyring and no
//! GUI session is involved and this runs in CI.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_sshelf");
const VAULT_PASS: &str = "askpass-plumbing-test";
const KEY_PASSPHRASE: &str = "the-key-passphrase";
const LOGIN_PASSWORD: &str = "the-login-password";

/// A throwaway XDG root plus the stub `ssh` that stands in for OpenSSH. Removed on drop.
struct Fixture {
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn unique(tag: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("sshelf-askpass-{}-{tag}-{n}", std::process::id()))
}

/// What the stub `ssh` recorded for one prompt.
#[derive(Debug, PartialEq, Eq)]
struct Reply {
    stdout: String,
    code: String,
}

impl Reply {
    /// The helper answered with this value.
    fn answered(&self, secret: &str) -> bool {
        self.code == "0" && self.stdout == secret
    }
    /// The helper refused: non-zero exit and nothing on stdout.
    fn declined(&self) -> bool {
        self.code == "1" && self.stdout.is_empty()
    }
    /// There was no helper wired at all.
    fn unwired(&self) -> bool {
        self.code == "99"
    }
}

impl Fixture {
    /// Build the XDG root, the host database, the stub `ssh`, and store both secrets.
    fn new(tag: &str, hosts_toml: &str) -> Self {
        let root = unique(tag);
        let config = root.join(".config/sshelf");
        let data = root.join(".local/share/sshelf");
        let bin = root.join("bin");
        let out = root.join("out");
        for dir in [&config, &data, &bin, &out] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // A real file to point `identity_files` at; its contents never matter here.
        std::fs::write(root.join("id_key"), b"not a real key\n").unwrap();
        std::fs::write(
            config.join("hosts.toml"),
            hosts_toml.replace("{ROOT}", &root.display().to_string()),
        )
        .unwrap();

        let stub = bin.join("ssh");
        std::fs::write(&stub, STUB_SSH).unwrap();
        set_executable(&stub);

        let f = Fixture { root };
        f.store_secret("keybox", KEY_PASSPHRASE);
        f.store_secret("pwbox", LOGIN_PASSWORD);
        f
    }

    fn out_dir(&self) -> PathBuf {
        self.root.join("out")
    }

    /// `sshelf`, wired to this fixture's XDG root and headless vault.
    fn sshelf(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join(".config"))
            .env("XDG_DATA_HOME", self.root.join(".local/share"))
            .env("SSHELF_VAULT_PASSPHRASE", VAULT_PASS)
            // Keep a developer's own settings out of the run.
            .env_remove("SSHELF_CONFIG")
            .env_remove("SSH_ASKPASS")
            .env_remove("SSH_ASKPASS_REQUIRE");
        c
    }

    fn store_secret(&self, host: &str, secret: &str) {
        let mut child = self
            .sshelf()
            .args(["set-password", host])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{secret}\n").as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "set-password {host} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Connect to `host`, letting the stub `ssh` interrogate the helper. Returns nothing: the
    /// answers are read back with [`Fixture::reply`] and [`Fixture::recorded`].
    fn connect(&self, host: &str) {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = self
            .sshelf()
            .arg(host)
            .env("PATH", path)
            .env("SSHELF_TEST_OUT", self.out_dir())
            .env("SSHELF_TEST_KEY", self.root.join("id_key"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "connect to {host} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn reply(&self, name: &str) -> Reply {
        Reply {
            stdout: self.recorded(&format!("{name}.out")),
            code: self.recorded(&format!("{name}.rc")),
        }
    }

    fn recorded(&self, name: &str) -> String {
        std::fs::read_to_string(self.out_dir().join(name))
            .unwrap_or_else(|e| panic!("the stub ssh never wrote {name}: {e}"))
    }

    /// The argv the stub `ssh` was given, one argument per line.
    fn argv(&self) -> Vec<String> {
        self.recorded("argv").lines().map(str::to_string).collect()
    }
}

fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Stands in for OpenSSH: records its argv, then asks the helper the four prompts that matter,
/// including the two a hostile endpoint would send. `99` means no helper was wired at all.
const STUB_SSH: &str = r#"#!/bin/sh
set -u
: > "$SSHELF_TEST_OUT/argv"
for a in "$@"; do printf '%s\n' "$a" >> "$SSHELF_TEST_OUT/argv"; done
printf '%s' "${SSHELF_HOST_ID:-}" > "$SSHELF_TEST_OUT/host_id"
printf '%s' "${SSHELF_SECRET_KIND:-}" > "$SSHELF_TEST_OUT/kind"

ask() {
  name="$1"; prompt="$2"
  if [ -z "${SSH_ASKPASS:-}" ]; then
    : > "$SSHELF_TEST_OUT/$name.out"
    printf '99' > "$SSHELF_TEST_OUT/$name.rc"
    return
  fi
  out=$("$SSH_ASKPASS" "$prompt" 2>/dev/null)
  rc=$?
  printf '%s' "$out" > "$SSHELF_TEST_OUT/$name.out"
  printf '%s' "$rc" > "$SSHELF_TEST_OUT/$name.rc"
}

ask password "Password:"
ask own_key "Enter passphrase for key '$SSHELF_TEST_KEY':"
ask other_key "Enter passphrase for key '/tmp/somebody-elses-key':"
ask code "Verification code: "
exit 0
"#;

const KEY_AND_PASSWORD_HOSTS: &str = r#"format_version = 1

[[host]]
id = "test-keyhost"
name = "keybox"
hostname = "127.0.0.1"
user = "tester"
auth = "key"
identity_files = ["{ROOT}/id_key"]

[[host]]
id = "test-pwhost"
name = "pwbox"
hostname = "127.0.0.1"
user = "tester"
auth = "password"
"#;

/// Finding H-02, the half that matters most: a server that asks `Password:` over
/// keyboard-interactive must not be handed the key passphrase.
#[test]
fn a_key_host_answers_only_its_own_key_prompt() {
    let f = Fixture::new("key", KEY_AND_PASSWORD_HOSTS);
    f.connect("keybox");

    assert!(
        f.reply("password").declined(),
        "a key host handed its passphrase to a password prompt: {:?}",
        f.reply("password")
    );
    assert!(
        f.reply("own_key").answered(KEY_PASSPHRASE),
        "the real key prompt was not answered: {:?}",
        f.reply("own_key")
    );
    assert!(
        f.reply("other_key").declined(),
        "a passphrase prompt for somebody else's key was answered: {:?}",
        f.reply("other_key")
    );
    // No code was queued, so the verification prompt gets nothing either.
    assert!(f.reply("code").declined());

    assert_eq!(f.recorded("kind"), "passphrase");
    let argv = f.argv();
    assert!(
        argv.windows(2)
            .any(|w| w == ["-o", "PreferredAuthentications=publickey"]),
        "a key host must pin public-key auth: {argv:?}"
    );
}

/// The mirror image: a password host answers `Password:` and refuses a passphrase prompt.
#[test]
fn a_password_host_answers_only_password_prompts() {
    let f = Fixture::new("password", KEY_AND_PASSWORD_HOSTS);
    f.connect("pwbox");

    assert!(
        f.reply("password").answered(LOGIN_PASSWORD),
        "the password prompt was not answered: {:?}",
        f.reply("password")
    );
    assert!(
        f.reply("own_key").declined(),
        "a password host answered a passphrase prompt: {:?}",
        f.reply("own_key")
    );
    assert!(f.reply("other_key").declined());
    assert!(f.reply("code").declined());

    assert_eq!(f.recorded("kind"), "password");
    let argv = f.argv();
    assert!(
        !argv
            .iter()
            .any(|a| a.starts_with("PreferredAuthentications")),
        "a password host must not be constrained: {argv:?}"
    );
}

const JUMP_HOSTS: &str = r#"format_version = 1

[[host]]
id = "test-keyhost"
name = "keybox"
hostname = "127.0.0.1"
user = "tester"
auth = "key"
identity_files = ["{ROOT}/id_key"]

[[host]]
id = "test-pwhost"
name = "pwbox"
hostname = "127.0.0.1"
user = "tester"
auth = "password"
jump_hosts = ["bastion.example.com"]

[[host]]
id = "test-hophost"
name = "hopbox"
hostname = "127.0.0.1"
user = "tester"
auth = "password"
jump_hosts = ["b1", "b2"]
"#;

/// One hop is constrained by an explicit ProxyCommand, so the helper can stay wired.
#[test]
fn one_jump_host_gets_a_proxy_command_and_keeps_the_helper() {
    let f = Fixture::new("jump", JUMP_HOSTS);
    f.connect("pwbox");

    let argv = f.argv();
    let proxy = argv
        .iter()
        .find(|a| a.starts_with("ProxyCommand="))
        .unwrap_or_else(|| panic!("no ProxyCommand in {argv:?}"));
    assert!(proxy.contains("BatchMode=yes"));
    assert!(proxy.contains("PasswordAuthentication=no"));
    assert!(proxy.contains("KbdInteractiveAuthentication=no"));
    assert!(proxy.ends_with("-W '[%h]:%p' bastion.example.com"));
    assert!(!argv.iter().any(|a| a == "-J"), "-J must be gone: {argv:?}");

    assert!(f.reply("password").answered(LOGIN_PASSWORD));
}

/// A chain sshelf cannot constrain gets no helper at all: `ssh` asks on the terminal instead of
/// handing the target's secret to a hop.
#[test]
fn a_multi_hop_jump_wires_no_helper_at_all() {
    let f = Fixture::new("multihop", JUMP_HOSTS);
    f.connect("hopbox");

    let argv = f.argv();
    let j = argv
        .iter()
        .position(|a| a == "-J")
        .unwrap_or_else(|| panic!("-J must be kept: {argv:?}"));
    assert_eq!(argv[j + 1], "b1,b2");
    assert!(!argv.iter().any(|a| a.starts_with("ProxyCommand=")));

    assert_eq!(f.recorded("host_id"), "", "the host id must not be wired");
    assert_eq!(f.recorded("kind"), "");
    for prompt in ["password", "own_key", "other_key", "code"] {
        assert!(
            f.reply(prompt).unwired(),
            "{prompt} reached a helper that should not exist: {:?}",
            f.reply(prompt)
        );
    }
}
