//! `sshelf doctor` — one command that checks the things that generate support questions.
//!
//! Scope, deliberately narrow (see `docs/decisions.md` D-027):
//!
//! - **Local only.** Every check reads the filesystem, the environment, the secret backend, or
//!   `ssh -V`. Nothing connects to anything: sshelf's no-network posture is not relaxed for
//!   diagnostics, so `doctor` never tells you whether a *host* is up — only whether *sshelf*
//!   is set up.
//! - **Read-only**, with exactly one exception: the keyring probe writes and immediately
//!   deletes a clearly-named throwaway entry, because a backend that can be read but not
//!   written is a backend that will fail the first time you save a password
//!   ([`crate::secrets::probe`]).
//! - **Exit 0 unless something failed.** Warnings don't fail the run; they're things worth
//!   knowing that still work today.
//!
//! Every check is a pure function over already-loaded inputs, so the whole matrix is
//! fixture-tested without a keyring, a config directory, or an `ssh` binary.

use std::path::Path;

use crate::model::{AuthMethod, Host, HostsFile};

/// How a single check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Nothing to do.
    Ok,
    /// Works today, but will bite — or already limits something.
    Warn,
    /// Broken now. Any of these makes the run exit 1.
    Fail,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
        }
    }
}

/// One line of the report: what was checked, what was found, and — when it isn't `ok` — the
/// single next action, on its own indented line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub level: Level,
    pub summary: String,
    pub remedy: Option<String>,
}

impl Check {
    fn ok(summary: impl Into<String>) -> Self {
        Check {
            level: Level::Ok,
            summary: summary.into(),
            remedy: None,
        }
    }

    fn warn(summary: impl Into<String>, remedy: impl Into<String>) -> Self {
        Check {
            level: Level::Warn,
            summary: summary.into(),
            remedy: Some(remedy.into()),
        }
    }

    fn fail(summary: impl Into<String>, remedy: impl Into<String>) -> Self {
        Check {
            level: Level::Fail,
            summary: summary.into(),
            remedy: Some(remedy.into()),
        }
    }

    /// The report lines for this check: the verdict, plus the remedy indented under it.
    pub fn render(&self) -> String {
        let mut out = format!("  {:<5} {}", self.level.label(), self.summary);
        if let Some(remedy) = &self.remedy {
            out.push_str(&format!("\n        → {remedy}"));
        }
        out
    }
}

/// The OpenSSH release that added `SSH_ASKPASS_REQUIRE`, which every stored secret rides on.
const MIN_OPENSSH: (u32, u32) = (8, 4);

/// Check 1 — the OpenSSH version, parsed from `ssh -V` (which prints to stderr).
///
/// Older than 8.4 is a **fail**: password and passphrase auto-supply simply cannot work, since
/// `SSH_ASKPASS_REQUIRE=force` doesn't exist. Output we can't parse is a **warn**, not a fail —
/// an unusual build string is not evidence of a broken client.
pub fn check_ssh_version(version_output: Option<&str>) -> Check {
    let Some(output) = version_output else {
        return Check::fail(
            "ssh — could not run `ssh -V`",
            "Install an OpenSSH client (`openssh-client` / `openssh-clients`); sshelf runs \
             `ssh` for every connection.",
        );
    };
    let Some((major, minor)) = parse_openssh_version(output) else {
        return Check::warn(
            format!("ssh — could not read a version from {:?}", output.trim()),
            "Check `ssh -V` yourself: sshelf needs OpenSSH 8.4+ for stored passwords and \
             passphrases (keys and agents work with anything).",
        );
    };
    if (major, minor) < MIN_OPENSSH {
        return Check::fail(
            format!("ssh — OpenSSH {major}.{minor} is older than 8.4"),
            "Upgrade OpenSSH: below 8.4 there is no SSH_ASKPASS_REQUIRE, so stored passwords \
             and key passphrases cannot be supplied automatically.",
        );
    }
    Check::ok(format!(
        "ssh — OpenSSH {major}.{minor} (8.4+, so stored secrets can be supplied)"
    ))
}

/// Pull `(major, minor)` out of an `ssh -V` line such as `OpenSSH_9.8p1, LibreSSL 3.3.6`.
fn parse_openssh_version(output: &str) -> Option<(u32, u32)> {
    let rest = output.split("OpenSSH_").nth(1)?;
    let mut digits = rest.split('.');
    let major: u32 = take_leading_number(digits.next()?)?;
    let minor: u32 = take_leading_number(digits.next()?)?;
    Some((major, minor))
}

fn take_leading_number(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Check 2 — the host database parses, and no two hosts collide.
///
/// Duplicate **ids** are a fail because the id keys both the secret store and frecency: two
/// hosts sharing one would share a password and a usage count. Duplicate **names** are a fail
/// because `sshelf <name>` resolves by exact match and silently takes the first.
pub fn check_hosts_file(loaded: Result<&HostsFile, &str>, path: &Path) -> Check {
    let file = match loaded {
        Ok(file) => file,
        Err(e) => {
            // The error already names the file (it carries the read/parse context), so the
            // summary doesn't repeat it — and a TOML error's caret diagram is folded away, so
            // one check stays one line.
            return Check::fail(
                format!("host database — could not be read: {}", single_line(e)),
                "Fix the TOML by hand (the message names the line and column), or move the file \
                 aside and re-import; sshelf never rewrites a file it can't parse.",
            );
        }
    };
    let dup_names = duplicates(file.hosts.iter().map(|h| h.name.as_str()));
    let dup_ids = duplicates(file.hosts.iter().map(|h| h.id.as_str()));
    if !dup_names.is_empty() || !dup_ids.is_empty() {
        let mut parts = Vec::new();
        if !dup_names.is_empty() {
            parts.push(format!("duplicate name(s): {}", dup_names.join(", ")));
        }
        if !dup_ids.is_empty() {
            parts.push(format!("duplicate id(s): {}", dup_ids.join(", ")));
        }
        return Check::fail(
            format!("host database — {}", parts.join("; ")),
            format!(
                "Edit {} and make each name and id unique — an id is shared with that host's \
                 stored password and usage history.",
                path.display()
            ),
        );
    }
    Check::ok(format!(
        "host database — {} host(s), {} site(s) in {}",
        file.hosts.len(),
        file.sites.len(),
        path.display()
    ))
}

/// Squash a multi-line error into one report line.
///
/// `toml`'s parse errors span several lines: a heading, a caret diagram pointing into the
/// source, then the reason. The diagram lines are the ones containing `|`; dropping them keeps
/// the two lines that matter (*where* and *what*) and leaves single-line errors untouched.
fn single_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains('|'))
        .collect::<Vec<_>>()
        .join(" — ")
}

/// Values that appear more than once, in first-seen order.
fn duplicates<'a>(items: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut dup = Vec::new();
    for item in items {
        if !seen.insert(item) && !dup.iter().any(|d| d == item) {
            dup.push(item.to_string());
        }
    }
    dup
}

/// Check 3 — every `site = "…"` names a site that actually exists.
///
/// A dangling name degrades to pure grouping rather than erroring, which is exactly why it
/// deserves a check: the host still connects, just without the bastion, user, or identity the
/// site was supposed to supply.
pub fn check_sites(file: &HostsFile) -> Check {
    let dangling: Vec<String> = file
        .hosts
        .iter()
        .filter_map(|h| {
            let site = h.site.as_deref()?;
            crate::model::find_site(&file.sites, site)
                .is_none()
                .then(|| format!("{} → \"{site}\"", h.name))
        })
        .collect();
    if dangling.is_empty() {
        return Check::ok("sites — every host's site is defined");
    }
    let missing: Vec<&str> = file
        .hosts
        .iter()
        .filter_map(|h| h.site.as_deref())
        .filter(|s| crate::model::find_site(&file.sites, s).is_none())
        .collect();
    let first = missing.first().copied().unwrap_or("NAME");
    Check::fail(
        format!(
            "sites — {} host(s) point at a site that isn't defined: {}",
            dangling.len(),
            dangling.join(", ")
        ),
        format!(
            "Define it with `sshelf sites add {first}`, or clear the field with Ctrl-e — until \
             then those hosts inherit nothing from it."
        ),
    )
}

/// Check 4 — the secret backend can actually be used.
pub fn check_secret_backend(backend: crate::secrets::Backend, probe: Result<(), String>) -> Check {
    let name = backend.as_str();
    match probe {
        Ok(()) => Check::ok(format!("secrets — the {name} is reachable")),
        Err(e) => {
            let remedy = match backend {
                crate::secrets::Backend::Keyring => {
                    "Start (or install) a Secret Service provider such as gnome-keyring, or set \
                     $SSHELF_VAULT_PASSPHRASE to use the encrypted vault file instead."
                }
                crate::secrets::Backend::Vault => {
                    "Fix $SSHELF_VAULT_PASSPHRASE, or move the vault aside and re-add secrets \
                     with `sshelf set-password` — nothing can be read out of it as it stands."
                }
            };
            Check::fail(format!("secrets — the {name} is not usable: {e}"), remedy)
        }
    }
}

/// Check 5 — stored secrets whose host is gone.
///
/// `stored` is `None` when the backend can't be listed at all, which is the normal case: the
/// OS keyring offers no portable enumeration, so this check only has teeth in vault mode.
/// Saying so plainly beats reporting a clean bill of health nobody actually checked.
pub fn check_orphaned_secrets(stored: Option<&[String]>, file: &HostsFile) -> Check {
    let Some(stored) = stored else {
        return Check::ok(
            "orphaned secrets — not checked (the OS keyring can't be listed; vault mode can)",
        );
    };
    let known: std::collections::HashSet<&str> = file.hosts.iter().map(|h| h.id.as_str()).collect();
    let orphans: Vec<&str> = stored
        .iter()
        .map(String::as_str)
        .filter(|id| !known.contains(id))
        .collect();
    if orphans.is_empty() {
        return Check::ok(format!(
            "orphaned secrets — none ({} stored, all matching a host)",
            stored.len()
        ));
    }
    Check::warn(
        format!(
            "orphaned secrets — {} stored secret(s) belong to no host: {}",
            orphans.len(),
            orphans.join(", ")
        ),
        "Harmless, but they outlive their host: delete the vault file to clear them all, or \
         re-add a host with that id and delete it from the TUI (Ctrl-d), which removes its \
         secret too.",
    )
}

/// Check 6 — an ssh-agent is actually reachable for the hosts that rely on one.
///
/// `auth_sock` is `$SSH_AUTH_SOCK`. Checking that the path *exists* is still a local
/// filesystem read (no connection is made), and a stale socket left behind by an ended session
/// is the more common failure of the two.
pub fn check_agent(hosts: &[Host], auth_sock: Option<&str>) -> Check {
    let agent_hosts: Vec<&str> = hosts
        .iter()
        .filter(|h| h.auth == AuthMethod::Agent)
        .map(|h| h.name.as_str())
        .collect();
    if agent_hosts.is_empty() {
        return Check::ok("ssh-agent — not needed (no host uses agent auth)");
    }
    let listed = agent_hosts.join(", ");
    match auth_sock.filter(|s| !s.is_empty()) {
        None => Check::warn(
            format!(
                "ssh-agent — $SSH_AUTH_SOCK is not set, but {} host(s) use agent auth: {listed}",
                agent_hosts.len()
            ),
            "Start an agent (`eval $(ssh-agent)`, or your desktop's) and `ssh-add` your key — \
             otherwise those hosts fall back to whatever else ssh can find.",
        ),
        Some(sock) if !Path::new(sock).exists() => Check::warn(
            format!("ssh-agent — $SSH_AUTH_SOCK points at {sock}, which doesn't exist"),
            "The agent that created that socket is gone (a stale value inherited from an older \
             session). Start a fresh agent, or open a new shell.",
        ),
        Some(_) => Check::ok(format!(
            "ssh-agent — $SSH_AUTH_SOCK is set for {} agent host(s)",
            agent_hosts.len()
        )),
    }
}

/// Check 7 — the exported ssh_config fragment still matches the database.
///
/// `existing` is the fragment's current content, or `None` when export isn't enabled (which is
/// the opt-out, not a problem). The comparison is against a freshly rendered fragment rather
/// than a timestamp: the render is deterministic, so identical content *is* an up-to-date
/// file, whatever the mtimes say.
pub fn check_export(existing: Option<&str>, fresh: &str, path: &Path) -> Check {
    match existing {
        None => Check::ok("ssh_config export — not enabled (run `sshelf export` to turn it on)"),
        Some(current) if current == fresh => Check::ok(format!(
            "ssh_config export — up to date ({})",
            path.display()
        )),
        Some(_) => Check::warn(
            format!(
                "ssh_config export — {} no longer matches your hosts",
                path.display()
            ),
            "Run `sshelf export` to regenerate it — it normally refreshes on every hosts \
             change, so either a save failed or the file was edited by hand.",
        ),
    }
}

/// Everything the report needs, gathered by the caller so every check stays pure.
pub struct Inputs<'a> {
    pub ssh_version: Option<String>,
    pub hosts: Result<&'a HostsFile, String>,
    pub hosts_path: &'a Path,
    pub backend: crate::secrets::Backend,
    pub probe: Result<(), String>,
    pub stored_ids: Option<Vec<String>>,
    pub auth_sock: Option<String>,
    pub export_existing: Option<String>,
    pub export_fresh: String,
    pub export_path: &'a Path,
}

/// Run every check, in the order they're reported.
///
/// The host database comes first among the checks that can block others: when it can't be
/// read, the checks that need it say so once instead of each inventing its own failure.
pub fn run(input: &Inputs) -> Vec<Check> {
    let mut checks = vec![
        check_ssh_version(input.ssh_version.as_deref()),
        check_hosts_file(
            input.hosts.as_ref().map_err(String::as_str).copied(),
            input.hosts_path,
        ),
        check_secret_backend(input.backend, input.probe.clone()),
    ];
    match &input.hosts {
        Ok(file) => {
            checks.push(check_sites(file));
            checks.push(check_orphaned_secrets(input.stored_ids.as_deref(), file));
            checks.push(check_agent(&file.hosts, input.auth_sock.as_deref()));
            checks.push(check_export(
                input.export_existing.as_deref(),
                &input.export_fresh,
                input.export_path,
            ));
        }
        Err(_) => checks.push(Check::warn(
            "sites, orphaned secrets, ssh-agent and export — not checked",
            "These all read the host database, which couldn't be parsed (see above). Fix that \
             first and run `sshelf doctor` again.",
        )),
    }
    checks
}

/// `n failed, n warnings, n ok` — the last line of the report.
pub fn summary(checks: &[Check]) -> String {
    let count = |level: Level| checks.iter().filter(|c| c.level == level).count();
    format!(
        "{} failed, {} warning(s), {} ok",
        count(Level::Fail),
        count(Level::Warn),
        count(Level::Ok)
    )
}

/// True when nothing failed — warnings are allowed. Drives the exit code.
pub fn healthy(checks: &[Check]) -> bool {
    !checks.iter().any(|c| c.level == Level::Fail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CURRENT_FORMAT_VERSION, Site};
    use std::path::PathBuf;

    fn file_with(hosts: Vec<Host>, sites: Vec<Site>) -> HostsFile {
        HostsFile {
            format_version: CURRENT_FORMAT_VERSION,
            sites,
            hosts,
        }
    }

    fn path() -> PathBuf {
        PathBuf::from("/home/u/.config/sshelf/hosts.toml")
    }

    #[test]
    fn ssh_version_is_parsed_from_the_real_banner_shapes() {
        assert_eq!(
            parse_openssh_version("OpenSSH_9.8p1, LibreSSL 3.3.6\n"),
            Some((9, 8))
        );
        assert_eq!(
            parse_openssh_version("OpenSSH_8.4p1 Debian-5+deb11u3, OpenSSL 1.1.1w\n"),
            Some((8, 4))
        );
        assert_eq!(
            parse_openssh_version("OpenSSH_10.0p2, OpenSSL 3.5.0\n"),
            Some((10, 0))
        );
        assert_eq!(parse_openssh_version("Dropbear v2022.83"), None);
        assert_eq!(parse_openssh_version(""), None);
    }

    #[test]
    fn ssh_version_gate_matches_the_askpass_floor() {
        assert_eq!(
            check_ssh_version(Some("OpenSSH_9.8p1, LibreSSL 3.3.6")).level,
            Level::Ok
        );
        // Exactly the floor passes.
        assert_eq!(
            check_ssh_version(Some("OpenSSH_8.4p1, OpenSSL 1.1.1")).level,
            Level::Ok
        );
        let old = check_ssh_version(Some("OpenSSH_8.3p1, OpenSSL 1.1.1"));
        assert_eq!(old.level, Level::Fail);
        assert!(old.summary.contains("8.3"));
        assert!(old.remedy.unwrap().contains("Upgrade OpenSSH"));
        // Unreadable output is a warning, not a verdict on the client.
        assert_eq!(
            check_ssh_version(Some("Dropbear v2022.83")).level,
            Level::Warn
        );
        assert_eq!(check_ssh_version(None).level, Level::Fail);
    }

    #[test]
    fn a_clean_hosts_file_passes_and_reports_its_size() {
        let file = file_with(
            vec![Host::new("web", "10.0.0.1"), Host::new("db", "10.0.0.2")],
            vec![Site::new("prod")],
        );
        let check = check_hosts_file(Ok(&file), &path());
        assert_eq!(check.level, Level::Ok);
        assert!(check.summary.contains("2 host(s), 1 site(s)"));
    }

    #[test]
    fn duplicate_names_and_ids_fail_and_are_named() {
        let mut a = Host::new("web", "10.0.0.1");
        let mut b = Host::new("web", "10.0.0.2");
        a.id = "SAME".into();
        b.id = "SAME".into();
        let file = file_with(vec![a, b], vec![]);
        let check = check_hosts_file(Ok(&file), &path());
        assert_eq!(check.level, Level::Fail);
        assert!(
            check.summary.contains("duplicate name(s): web"),
            "{check:?}"
        );
        assert!(check.summary.contains("duplicate id(s): SAME"), "{check:?}");
        assert!(check.remedy.unwrap().contains("unique"));
    }

    #[test]
    fn an_unparseable_hosts_file_fails_with_a_one_line_parse_error() {
        // Exactly the shape `toml` produces, caret diagram and all.
        let raw = "parsing /h/hosts.toml: TOML parse error at line 2, column 7\n  |\n2 |                    [[host\n  |       ^\nunclosed array table, expected `]]`\n";
        let check = check_hosts_file(Err(raw), &path());
        assert_eq!(check.level, Level::Fail);
        assert_eq!(
            check.summary,
            "host database — could not be read: parsing /h/hosts.toml: TOML parse error at \
             line 2, column 7 — unclosed array table, expected `]]`"
        );
        assert_eq!(check.summary.lines().count(), 1, "one check, one line");
        assert!(check.remedy.unwrap().contains("line and column"));
    }

    #[test]
    fn single_line_keeps_single_line_errors_intact() {
        assert_eq!(
            single_line("Permission denied (os error 13)"),
            "Permission denied (os error 13)"
        );
        assert_eq!(single_line("  padded  \n\n"), "padded");
        assert_eq!(single_line(""), "");
    }

    #[test]
    fn a_dangling_site_reference_fails_and_names_a_runnable_fix() {
        let mut h = Host::new("web", "10.0.0.1");
        h.site = Some("prod-dc".into());
        let file = file_with(vec![h], vec![Site::new("staging")]);
        let check = check_sites(&file);
        assert_eq!(check.level, Level::Fail);
        assert!(check.summary.contains("web → \"prod-dc\""));
        assert_eq!(
            check.remedy.unwrap(),
            "Define it with `sshelf sites add prod-dc`, or clear the field with Ctrl-e — until \
             then those hosts inherit nothing from it."
        );
    }

    #[test]
    fn a_defined_site_passes_case_insensitively() {
        let mut h = Host::new("web", "10.0.0.1");
        h.site = Some("PROD".into());
        let file = file_with(vec![h], vec![Site::new("prod")]);
        assert_eq!(check_sites(&file).level, Level::Ok);
    }

    #[test]
    fn orphaned_secrets_are_named_when_the_backend_can_be_listed() {
        let mut h = Host::new("web", "10.0.0.1");
        h.id = "KEPT".into();
        let file = file_with(vec![h], vec![]);

        let clean = check_orphaned_secrets(Some(&["KEPT".to_string()]), &file);
        assert_eq!(clean.level, Level::Ok);

        let orphaned =
            check_orphaned_secrets(Some(&["KEPT".to_string(), "GONE".to_string()]), &file);
        assert_eq!(orphaned.level, Level::Warn);
        assert!(orphaned.summary.contains("GONE"));
        assert!(!orphaned.summary.contains("KEPT"));
    }

    #[test]
    fn an_unlistable_backend_says_so_rather_than_reporting_a_clean_bill() {
        let file = file_with(vec![Host::new("web", "h")], vec![]);
        let check = check_orphaned_secrets(None, &file);
        assert_eq!(check.level, Level::Ok);
        assert!(check.summary.contains("not checked"), "{check:?}");
        assert!(check.summary.contains("can't be listed"));
    }

    #[test]
    fn the_agent_check_only_fires_for_agent_hosts() {
        let mut key_host = Host::new("web", "h");
        key_host.auth = AuthMethod::Key;
        assert_eq!(check_agent(&[key_host], None).level, Level::Ok);

        // Agent auth is the default, so a bare host counts.
        let agent_host = Host::new("box", "h");
        let missing = check_agent(std::slice::from_ref(&agent_host), None);
        assert_eq!(missing.level, Level::Warn);
        assert!(missing.summary.contains("box"), "{missing:?}");
        assert!(missing.remedy.unwrap().contains("ssh-agent"));

        // An empty value is the same as unset.
        assert_eq!(
            check_agent(std::slice::from_ref(&agent_host), Some("")).level,
            Level::Warn
        );
        // A socket path that isn't there is a stale inherited value.
        let stale = check_agent(
            std::slice::from_ref(&agent_host),
            Some("/tmp/definitely-not-a-socket-9f3a"),
        );
        assert_eq!(stale.level, Level::Warn);
        assert!(stale.summary.contains("doesn't exist"));
        // A real path passes (this file exists on every unix).
        assert_eq!(
            check_agent(std::slice::from_ref(&agent_host), Some("/dev/null")).level,
            Level::Ok
        );
    }

    #[test]
    fn export_staleness_is_content_based() {
        let p = PathBuf::from("/home/u/.config/sshelf/ssh_config");
        assert_eq!(check_export(None, "fresh", &p).level, Level::Ok);
        assert_eq!(check_export(Some("same"), "same", &p).level, Level::Ok);
        let stale = check_export(Some("old"), "new", &p);
        assert_eq!(stale.level, Level::Warn);
        assert!(stale.remedy.unwrap().contains("`sshelf export`"));
    }

    #[test]
    fn the_secret_backend_remedy_matches_the_backend_in_use() {
        use crate::secrets::Backend;
        assert_eq!(
            check_secret_backend(Backend::Keyring, Ok(())).level,
            Level::Ok
        );
        let keyring = check_secret_backend(Backend::Keyring, Err("no Secret Service".into()));
        assert_eq!(keyring.level, Level::Fail);
        assert!(keyring.summary.contains("OS keyring"));
        assert!(keyring.remedy.unwrap().contains("SSHELF_VAULT_PASSPHRASE"));

        let vault = check_secret_backend(Backend::Vault, Err("wrong passphrase?".into()));
        assert!(vault.summary.contains("age vault"));
        assert!(vault.remedy.unwrap().contains("$SSHELF_VAULT_PASSPHRASE"));
    }

    #[test]
    fn exit_code_ignores_warnings_but_not_failures() {
        assert!(healthy(&[Check::ok("a"), Check::warn("b", "c")]));
        assert!(!healthy(&[Check::ok("a"), Check::fail("b", "c")]));
        assert_eq!(
            summary(&[Check::ok("a"), Check::warn("b", "c"), Check::fail("d", "e")]),
            "1 failed, 1 warning(s), 1 ok"
        );
    }

    #[test]
    fn every_not_ok_check_carries_exactly_one_next_action() {
        let mut broken = Host::new("web", "10.0.0.1");
        broken.site = Some("ghost".into());
        let file = file_with(vec![broken], vec![]);
        let checks = run(&Inputs {
            ssh_version: Some("OpenSSH_7.9p1, LibreSSL 2.7.3".into()),
            hosts: Ok(&file),
            hosts_path: &path(),
            backend: crate::secrets::Backend::Vault,
            probe: Err("could not decrypt vault".into()),
            stored_ids: Some(vec!["GONE".into()]),
            auth_sock: None,
            export_existing: Some("stale".into()),
            export_fresh: "fresh".into(),
            export_path: &PathBuf::from("/x/ssh_config"),
        });
        assert_eq!(checks.len(), 7);
        for check in &checks {
            match check.level {
                Level::Ok => assert!(check.remedy.is_none(), "{check:?}"),
                _ => {
                    let remedy = check.remedy.as_deref().unwrap_or_default();
                    assert!(!remedy.is_empty(), "{check:?} needs a next action");
                    // A remedy is a sentence, not a shrug.
                    assert!(remedy.ends_with('.'), "{check:?}");
                }
            }
        }
        assert!(!healthy(&checks));
    }

    #[test]
    fn an_unreadable_hosts_file_reports_once_instead_of_seven_times() {
        let checks = run(&Inputs {
            ssh_version: Some("OpenSSH_9.8p1, LibreSSL 3.3.6".into()),
            hosts: Err("parsing hosts.toml: expected `=` at line 4".into()),
            hosts_path: &path(),
            backend: crate::secrets::Backend::Keyring,
            probe: Ok(()),
            stored_ids: None,
            auth_sock: Some("/dev/null".into()),
            export_existing: None,
            export_fresh: String::new(),
            export_path: &PathBuf::from("/x/ssh_config"),
        });
        assert_eq!(checks.len(), 4);
        assert_eq!(
            checks.iter().filter(|c| c.level == Level::Fail).count(),
            1,
            "one root cause, one failure"
        );
        assert!(checks.last().unwrap().summary.contains("not checked"));
    }

    #[test]
    fn a_healthy_setup_reports_nothing_to_do() {
        let file = file_with(vec![Host::new("web", "10.0.0.1")], vec![]);
        let checks = run(&Inputs {
            ssh_version: Some("OpenSSH_9.8p1, LibreSSL 3.3.6".into()),
            hosts: Ok(&file),
            hosts_path: &path(),
            backend: crate::secrets::Backend::Keyring,
            probe: Ok(()),
            stored_ids: None,
            auth_sock: Some("/dev/null".into()),
            export_existing: None,
            export_fresh: String::new(),
            export_path: &PathBuf::from("/x/ssh_config"),
        });
        assert!(healthy(&checks));
        assert!(checks.iter().all(|c| c.remedy.is_none()));
        assert_eq!(summary(&checks), "0 failed, 0 warning(s), 7 ok");
    }

    #[test]
    fn a_rendered_check_puts_the_remedy_on_its_own_indented_line() {
        let rendered = Check::fail("thing is broken", "Do this.").render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], "  fail  thing is broken");
        assert_eq!(lines[1], "        → Do this.");
        assert_eq!(Check::ok("fine").render(), "  ok    fine");
    }
}
