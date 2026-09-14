//! `sshelf add --from-ssh`, driven through the real binary against a throwaway `--config`. No
//! server is involved: this is the parser, the flag routing, and the store, end to end.

#![cfg(unix)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_sshelf");
const VAULT_PASS: &str = "add-from-ssh-test";

/// A throwaway home: config under `cfg/`, data under `.local/share`. Removed on drop.
struct Root(PathBuf);

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

impl Root {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("sshelf-fromssh-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        Root(root)
    }

    fn env(&self, cmd: &mut Command) {
        cmd.env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", self.0.join(".config"))
            .env("XDG_DATA_HOME", self.0.join(".local/share"))
            .env("SSHELF_VAULT_PASSPHRASE", VAULT_PASS)
            .env_remove("SSHELF_CONFIG")
            .env_remove("XDG_RUNTIME_DIR");
    }

    /// Run `sshelf --config <root>/cfg/config.toml <args>` with `stdin` piped in.
    fn sshelf(&self, args: &[&str], stdin: &str) -> Output {
        let mut cmd = Command::new(BIN);
        cmd.arg("--config")
            .arg(self.0.join("cfg/config.toml"))
            .args(args);
        self.env(&mut cmd);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn hosts_toml(&self) -> Option<String> {
        std::fs::read_to_string(self.0.join("cfg/hosts.toml")).ok()
    }

    fn hosts(&self) -> Vec<toml::Table> {
        let Some(raw) = self.hosts_toml() else {
            return Vec::new();
        };
        let file: toml::Table = toml::from_str(&raw).unwrap();
        file.get("host")
            .and_then(|h| h.as_array())
            .map(|a| a.iter().map(|h| h.as_table().unwrap().clone()).collect())
            .unwrap_or_default()
    }

    fn host(&self, name: &str) -> toml::Table {
        self.hosts()
            .into_iter()
            .find(|h| h["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("no host named {name} in {:?}", self.hosts_toml()))
    }

    /// What the askpass helper would hand ssh for this password host: the stored secret.
    fn stored_password(&self, id: &str) -> Option<String> {
        let mut cmd = Command::new(BIN);
        cmd.arg("Password:")
            .env("SSHELF_ASKPASS", "1")
            .env("SSHELF_HOST_ID", id)
            .env("SSHELF_SECRET_KIND", "password");
        self.env(&mut cmd);
        let out = cmd.output().unwrap();
        out.status
            .success()
            .then(|| text(&out.stdout).trim_end_matches('\n').to_string())
    }
}

#[test]
fn the_motivating_line_adds_a_key_host_quietly() {
    let r = Root::new("motivating");
    let line = "ssh -i ~/Downloads/dev-ooblek-privco.pem -o StrictHostKeyChecking=no ubuntu@44.196.235.116";
    let out = r.sshelf(&["add", "--from-ssh", line, "--quiet"], "");
    assert!(out.status.success(), "{}", text(&out.stderr));

    // The drop note comes first, then the one line a normal add prints.
    let stdout = text(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{stdout}");
    assert_eq!(
        lines[0],
        "note: dropped -o StrictHostKeyChecking=no: sshelf passes accept-new on every connect"
    );
    assert!(lines[1].starts_with("added '44.196.235.116' ("), "{stdout}");

    let h = r.host("44.196.235.116");
    assert_eq!(h["hostname"].as_str(), Some("44.196.235.116"));
    assert_eq!(h["user"].as_str(), Some("ubuntu"));
    assert_eq!(h["auth"].as_str(), Some("key"));
    assert_eq!(
        h["identity_files"],
        toml::Value::Array(vec!["~/Downloads/dev-ooblek-privco.pem".into()])
    );
    assert!(!h.contains_key("extra_args"), "{h:?}");
    assert!(!h.contains_key("port"), "{h:?}");

    let out = r.sshelf(&["print-command", "44.196.235.116"], "");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let key = r.0.join("Downloads/dev-ooblek-privco.pem");
    assert_eq!(
        shlex::split(text(&out.stdout).trim_end()).unwrap(),
        vec![
            "ssh",
            "-i",
            key.to_str().unwrap(),
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "PreferredAuthentications=publickey",
            "ubuntu@44.196.235.116",
        ]
    );

    // The same line again: the name is taken, and the way out is named.
    let out = r.sshelf(&["add", "--from-ssh", line, "--quiet"], "");
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("sshelf add NAME --from-ssh"),
        "{}",
        text(&out.stderr)
    );
    // With a name in front it goes in.
    let out = r.sshelf(&["add", "ooblek-dev", "--from-ssh", line, "-q"], "");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(r.hosts().len(), 2);
}

/// `--from-ssh` and `--password-stdin` both read stdin: the command line is the first line and
/// the secret the second.
#[test]
fn stdin_carries_the_command_line_then_the_secret() {
    let r = Root::new("two-lines");
    let out = r.sshelf(
        &["add", "legacy", "--from-ssh", "--password-stdin", "--quiet"],
        "ssh -p 2222 ops@legacy.example\nhunter2 with a space \n",
    );
    assert!(out.status.success(), "{}", text(&out.stderr));

    let h = r.host("legacy");
    assert_eq!(h["hostname"].as_str(), Some("legacy.example"));
    assert_eq!(h["user"].as_str(), Some("ops"));
    assert_eq!(h["port"].as_integer(), Some(2222));
    assert_eq!(
        h["auth"].as_str(),
        Some("password"),
        "--password-stdin implies it"
    );
    let id = h["id"].as_str().unwrap();
    assert_eq!(
        r.stored_password(id).as_deref(),
        Some("hunter2 with a space ")
    );
    assert!(!r.hosts_toml().unwrap().contains("hunter2"));
}

#[test]
fn what_from_ssh_refuses_writes_nothing() {
    let r = Root::new("refused");

    let out = r.sshelf(&["add", "web", "-H", "h", "--quiet"], "");
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("--quiet only applies to --from-ssh"));

    let out = r.sshelf(&["add", "--from-ssh", "ssh u@h", "--port", "22"], "");
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("cannot be used with"), "{err}");
    assert!(
        err.contains("note: the ssh command line already supplies"),
        "{err}"
    );

    let out = r.sshelf(&["add", "--from-ssh", "ssh u@h uptime", "-q"], "");
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("runs a remote command (`uptime`)"), "{err}");

    let out = r.sshelf(&["add", "--from-ssh", "-q"], "sudo ssh u@h\n");
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("`sudo` is not ssh"));

    assert!(r.hosts().is_empty(), "{:?}", r.hosts_toml());
}
