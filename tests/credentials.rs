//! Shared credentials, driven through the real binary against a throwaway `--config` and a
//! throwaway age vault. No server is involved: this covers the CLI surface, the store, and the
//! one thing that actually matters at connect time, which is the key the askpass helper reads.

#![cfg(unix)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_sshelf");
const VAULT_PASS: &str = "credentials-test";

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
            std::env::temp_dir().join(format!("sshelf-creds-{}-{tag}-{n}", std::process::id()));
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

    /// Add a password host and return its id.
    fn add(&self, name: &str, hostname: &str) -> String {
        let out = self.sshelf(&["add", name, "-H", hostname, "--auth", "password"], "");
        assert!(out.status.success(), "{}", text(&out.stderr));
        let file: toml::Table = toml::from_str(&self.hosts_toml()).unwrap();
        file["host"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["name"].as_str() == Some(name))
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn hosts_toml(&self) -> String {
        std::fs::read_to_string(self.0.join("cfg/hosts.toml")).unwrap_or_default()
    }

    fn host_field(&self, name: &str, field: &str) -> Option<String> {
        let file: toml::Table = toml::from_str(&self.hosts_toml()).unwrap();
        file["host"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["name"].as_str() == Some(name))
            .and_then(|h| h.get(field))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// Exactly what a connect hands the askpass helper: the key, traded for the secret. This is
    /// the real contract of a shared credential, so the test asks the same way ssh would.
    fn secret_for_key(&self, key: &str) -> Option<String> {
        let mut cmd = Command::new(BIN);
        cmd.arg("Password:")
            .env("SSHELF_ASKPASS", "1")
            .env("SSHELF_HOST_ID", key)
            .env("SSHELF_SECRET_KIND", "password");
        self.env(&mut cmd);
        let out = cmd.output().unwrap();
        out.status
            .success()
            .then(|| text(&out.stdout).trim_end().to_string())
    }
}

#[test]
fn one_secret_serves_every_bound_host_and_rotates_once() {
    let r = Root::new("share");
    let web = r.add("web", "10.0.0.1");
    let db = r.add("db", "10.0.0.2");
    let lone = r.add("lone", "10.0.0.3");

    let out = r.sshelf(&["credentials", "add", "corp-ldap"], "");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = r.sshelf(&["credentials", "bind", "corp-ldap", "web", "db"], "");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("nothing stored for 'corp-ldap' yet"),
        "{}",
        text(&out.stdout)
    );

    // The pointer is in the file; the secret is not.
    assert_eq!(
        r.host_field("web", "credential").as_deref(),
        Some("corp-ldap")
    );
    assert_eq!(
        r.host_field("db", "credential").as_deref(),
        Some("corp-ldap")
    );
    assert_eq!(r.host_field("lone", "credential"), None);
    let toml = r.hosts_toml();
    assert!(toml.contains("format_version = 2"), "{toml}");
    assert!(!toml.contains("s3cret"), "{toml}");

    // One write, and both hosts can authenticate.
    let out = r.sshelf(&["set-password", "--credential", "corp-ldap"], "s3cret\n");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("all 2 hosts"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(
        r.secret_for_key("credential:corp-ldap").as_deref(),
        Some("s3cret")
    );
    // Nothing was written under either host's own id.
    assert_eq!(r.secret_for_key(&web), None);
    assert_eq!(r.secret_for_key(&db), None);

    // Naming a bound host sets the shared value, and says so.
    let out = r.sshelf(&["set-password", "web"], "rotated\n");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("all 2 hosts"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(
        r.secret_for_key("credential:corp-ldap").as_deref(),
        Some("rotated"),
        "one rotation covers the fleet"
    );

    // An unbound host still owns its own secret.
    let out = r.sshelf(&["set-password", "lone"], "mine\n");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(r.secret_for_key(&lone).as_deref(), Some("mine"));
    assert_eq!(
        r.secret_for_key("credential:corp-ldap").as_deref(),
        Some("rotated"),
        "and does not touch the shared one"
    );
}

#[test]
fn add_can_bind_at_creation_and_stores_the_secret_under_the_credential() {
    let r = Root::new("addbind");
    // One call per group, then one call per host: the shape a scripted rollout wants.
    let out = r.sshelf(
        &["credentials", "add", "corp-ldap", "--password-stdin"],
        "fleet-secret\n",
    );
    assert!(out.status.success(), "{}", text(&out.stderr));

    let out = r.sshelf(
        &[
            "add",
            "web-07",
            "-H",
            "10.0.0.7",
            "--auth",
            "password",
            "--credential",
            "corp-ldap",
        ],
        "",
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    // Already satisfied, so it must not tell you to go and set a password.
    assert!(
        stdout.contains("credential 'corp-ldap', which is set"),
        "{stdout}"
    );
    assert!(!stdout.contains("no password stored"), "{stdout}");

    assert_eq!(
        r.host_field("web-07", "credential").as_deref(),
        Some("corp-ldap")
    );
    assert_eq!(
        r.secret_for_key("credential:corp-ldap").as_deref(),
        Some("fleet-secret")
    );
}

#[test]
fn adding_with_a_credential_and_a_password_writes_the_shared_key_not_the_host_id() {
    // The bug this guards: `save_new_host` used to store under the host's own id, which a bound
    // host never reads, so the password would have been silently useless.
    let r = Root::new("addsecret");
    let out = r.sshelf(
        &[
            "add",
            "db-01",
            "-H",
            "10.0.0.8",
            "--credential",
            "corp",
            "--password-stdin",
        ],
        "from-the-add\n",
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        r.secret_for_key("credential:corp").as_deref(),
        Some("from-the-add"),
        "the secret must land where the host will look for it"
    );

    // And a second host on the same credential needs no secret of its own.
    let out = r.sshelf(
        &[
            "add",
            "db-02",
            "-H",
            "10.0.0.9",
            "--credential",
            "corp",
            "--auth",
            "password",
        ],
        "",
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("which is set"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn add_says_what_is_missing_when_the_credential_has_no_secret_yet() {
    let r = Root::new("addempty");
    let out = r.sshelf(
        &[
            "add",
            "web-01",
            "-H",
            "10.0.0.1",
            "--auth",
            "password",
            "-c",
            "not-set-yet",
        ],
        "",
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("has nothing stored yet"), "{stdout}");
    assert!(stdout.contains("--credential not-set-yet"), "{stdout}");
}

#[test]
fn a_bad_credential_name_on_add_writes_no_host_at_all() {
    let r = Root::new("addbad");
    let out = r.sshelf(
        &[
            "add",
            "web-01",
            "-H",
            "10.0.0.1",
            "--credential",
            "corp ldap",
        ],
        "",
    );
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("credential name cannot use"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(
        r.hosts_toml(),
        "",
        "nothing may be written when the name is refused"
    );
}

#[test]
fn credentials_add_with_empty_stdin_creates_nothing() {
    let r = Root::new("credempty");
    let out = r.sshelf(&["credentials", "add", "corp", "--password-stdin"], "");
    assert!(!out.status.success());
    assert_eq!(
        r.hosts_toml(),
        "",
        "the credential must not be declared either"
    );
}

#[test]
fn from_ssh_can_bind_at_creation_too() {
    let r = Root::new("fromssh");
    let out = r.sshelf(
        &[
            "add",
            "legacy",
            "--credential",
            "corp-ldap",
            "--quiet",
            "--from-ssh",
            "ssh -p 2222 ops@10.0.0.50",
        ],
        "",
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        r.host_field("legacy", "credential").as_deref(),
        Some("corp-ldap")
    );
    assert_eq!(
        r.host_field("legacy", "hostname").as_deref(),
        Some("10.0.0.50")
    );
}

#[test]
fn unbinding_leaves_the_credential_and_rebinding_finds_it_again() {
    let r = Root::new("unbind");
    r.add("web", "10.0.0.1");
    r.sshelf(&["credentials", "bind", "corp", "web"], "");
    r.sshelf(&["set-password", "--credential", "corp"], "shared\n");

    let out = r.sshelf(&["credentials", "unbind", "web"], "");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(r.host_field("web", "credential"), None);
    // The value survives, so rebinding does not mean retyping.
    assert_eq!(
        r.secret_for_key("credential:corp").as_deref(),
        Some("shared")
    );

    r.sshelf(&["credentials", "bind", "corp", "web"], "");
    assert_eq!(r.host_field("web", "credential").as_deref(), Some("corp"));
}

#[test]
fn binding_without_declaring_works_and_says_what_is_missing() {
    let r = Root::new("undeclared");
    r.add("web", "10.0.0.1");
    // No `credentials add` first: the table is metadata, not the lookup.
    let out = r.sshelf(&["credentials", "bind", "corp-ldap", "web"], "");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("no [[credential]] entry"), "{stdout}");

    // And it is still listed, flagged as undeclared.
    let out = r.sshelf(&["credentials"], "");
    let listing = text(&out.stdout);
    assert!(listing.contains("corp-ldap"), "{listing}");
    assert!(listing.contains("not declared"), "{listing}");
}

#[test]
fn removing_a_credential_in_use_is_refused_until_forced() {
    let r = Root::new("rm");
    r.add("web", "10.0.0.1");
    r.add("db", "10.0.0.2");
    r.sshelf(&["credentials", "add", "corp"], "");
    r.sshelf(&["credentials", "bind", "corp", "web", "db"], "");
    r.sshelf(&["set-password", "--credential", "corp"], "shared\n");

    let out = r.sshelf(&["credentials", "rm", "corp"], "");
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("2 host(s) still use"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(r.host_field("web", "credential").as_deref(), Some("corp"));

    let out = r.sshelf(&["credentials", "rm", "corp", "--force"], "");
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(r.host_field("web", "credential"), None);
    assert_eq!(r.host_field("db", "credential"), None);
    // Unreachable once undeclared, so it is removed rather than left as an orphan.
    assert_eq!(r.secret_for_key("credential:corp"), None);
}

#[test]
fn the_empty_listing_names_the_keys_that_actually_do_it() {
    let r = Root::new("emptykeys");
    let listing = text(&r.sshelf(&["credentials"], "").stdout);
    assert!(listing.contains("No shared credentials yet"), "{listing}");
    // The hint has to match the real bindings: ^s marks, ^k applies.
    assert!(listing.contains("^s"), "{listing}");
    assert!(listing.contains("^k"), "{listing}");
    assert!(!listing.contains("Space"), "{listing}");
    assert!(!listing.contains("^p"), "{listing}");
}

#[test]
fn the_listing_reports_whether_a_secret_is_there() {
    let r = Root::new("list");
    r.add("web", "10.0.0.1");
    r.sshelf(
        &["credentials", "add", "corp", "--note", "directory password"],
        "",
    );

    let listing = text(&r.sshelf(&["credentials"], "").stdout);
    assert!(listing.contains("0 host(s)"), "{listing}");
    assert!(listing.contains("no secret yet"), "{listing}");
    assert!(listing.contains("directory password"), "{listing}");

    r.sshelf(&["credentials", "bind", "corp", "web"], "");
    r.sshelf(&["set-password", "--credential", "corp"], "x\n");
    let listing = text(&r.sshelf(&["credentials"], "").stdout);
    assert!(listing.contains("1 host(s)"), "{listing}");
    assert!(listing.contains("secret stored"), "{listing}");

    // JSON carries the same facts and never the value.
    let json = text(&r.sshelf(&["credentials", "--json"], "").stdout);
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(rows[0]["name"], "corp");
    assert_eq!(rows[0]["secret_stored"], true);
    assert_eq!(rows[0]["hosts"][0], "web");
    assert!(
        !json.contains("\"x\""),
        "the value must never be in the listing:\n{json}"
    );

    // `credentials list --json` is the same thing spelled out.
    let explicit = text(&r.sshelf(&["credentials", "list", "--json"], "").stdout);
    assert_eq!(explicit, json);
}

#[test]
fn sites_json_works_without_naming_the_list_action() {
    // `docs/cli.md` has always documented `sshelf sites [--json]`; it used to be rejected.
    let r = Root::new("sitesjson");
    let out = r.sshelf(&["sites", "add", "prod-dc", "-u", "deploy"], "");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let json = text(&r.sshelf(&["sites", "--json"], "").stdout);
    let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(rows[0]["name"], "prod-dc");
    assert_eq!(rows[0]["user"], "deploy");
}

#[test]
fn a_bad_credential_name_is_refused_before_anything_is_written() {
    let r = Root::new("badname");
    r.add("web", "10.0.0.1");
    for bad in ["corp ldap", "corp:ldap", "corp/ldap", ""] {
        let out = r.sshelf(&["credentials", "add", bad], "");
        assert!(!out.status.success(), "{bad} should be refused");
    }
    let out = r.sshelf(&["credentials", "bind", "corp ldap", "web"], "");
    assert!(!out.status.success());
    assert_eq!(r.host_field("web", "credential"), None);
}

#[test]
fn binding_an_unknown_host_changes_nothing() {
    let r = Root::new("partial");
    r.add("web", "10.0.0.1");
    let out = r.sshelf(&["credentials", "bind", "corp", "web", "ghost"], "");
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("no host with name or id 'ghost'"));
    assert_eq!(
        r.host_field("web", "credential"),
        None,
        "a half-applied bind is worse than none"
    );
}

#[test]
fn a_remote_command_needs_a_host_and_a_double_dash() {
    let r = Root::new("remote");
    r.add("web", "10.0.0.1");

    // `--` with no host in front.
    let out = r.sshelf(&["--", "uptime"], "");
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("name a host before `--`"),
        "{}",
        text(&out.stderr)
    );

    // The old guard still catches a subcommand read as a host.
    let out = r.sshelf(&["web", "list"], "");
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("is read as a host name"),
        "{}",
        text(&out.stderr)
    );

    // -t only means something with a command.
    let out = r.sshelf(&["-t", "web"], "");
    assert!(!out.status.success());
}
