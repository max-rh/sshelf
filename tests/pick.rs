//! `sshelf pick`, driven through the real binary with a throwaway `--config`.
//!
//! The mode's whole contract is which stream carries what: the list draws on stderr, stdout
//! carries the chosen name and nothing else. Without a terminal to draw on there is nothing to
//! pick, so that is what these can check without a pty; the interactive side is covered by the
//! tmux run recorded in `docs/progress.md`.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_sshelf");

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
            std::env::temp_dir().join(format!("sshelf-pick-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        Root(root)
    }

    fn sshelf(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(BIN);
        cmd.arg("--config")
            .arg(self.0.join("cfg/config.toml"))
            .args(args)
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", self.0.join(".config"))
            .env("XDG_DATA_HOME", self.0.join(".local/share"))
            .env_remove("SSHELF_CONFIG");
        cmd.output().unwrap()
    }
}

#[test]
fn pick_without_a_terminal_fails_and_keeps_stdout_empty() {
    let root = Root::new("no-tty");
    let out = root.sshelf(&["add", "web-01", "-H", "10.0.0.1"]);
    assert!(out.status.success(), "{}", text(&out.stderr));

    // Captured output means no terminal on stderr, which is where the list would have gone.
    let out = root.sshelf(&["pick"]);
    assert!(!out.status.success(), "should refuse without a terminal");
    assert!(
        out.stdout.is_empty(),
        "stdout is the chosen name or nothing: {:?}",
        text(&out.stdout)
    );
    let err = text(&out.stderr);
    assert!(err.contains("stderr"), "{err}");
    assert!(err.contains("$(sshelf pick)"), "says how to use it: {err}");
}

#[test]
fn pick_takes_a_query_and_still_writes_nothing_to_stdout() {
    let root = Root::new("query");
    let out = root.sshelf(&["add", "web-01", "-H", "10.0.0.1"]);
    assert!(out.status.success(), "{}", text(&out.stderr));

    let out = root.sshelf(&["pick", "tag:prod", "web"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "{:?}", text(&out.stdout));
}

#[test]
fn pick_help_says_it_prints_rather_than_connects() {
    let root = Root::new("help");
    let out = root.sshelf(&["pick", "--help"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let help = text(&out.stdout);
    assert!(help.contains("without connecting"), "{help}");
    assert!(help.contains("stderr"), "{help}");
    // The two ways to change how it draws.
    assert!(help.contains("--height"), "{help}");
    assert!(help.contains("--full-screen"), "{help}");
}

#[test]
fn a_height_and_full_screen_together_are_refused() {
    let root = Root::new("conflict");
    let out = root.sshelf(&["pick", "--height", "8", "--full-screen"]);
    assert!(!out.status.success(), "one or the other, not both");
    assert!(out.stdout.is_empty());
}
