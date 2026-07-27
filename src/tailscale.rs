//! Opt-in inventory import from the user's own Tailscale tailnet.
//!
//! `sshelf import --tailscale` shells out to the **user's** `tailscale` CLI
//! (`tailscale status --json`) and maps every eligible peer to a host. sshelf still opens no
//! sockets of its own: the binary is run only by that subcommand — never at startup, on save,
//! on a timer, or from the TUI (see `docs/decisions.md` D-024).
//!
//! Everything except [`capture_status`] is pure and unit-tested against fixture JSON, so the
//! tests need neither a tailscale binary nor a network.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::import::ImportResult;
use crate::model::Host;

/// Override for the `tailscale` binary — nonstandard installs, and the test seam.
pub const BIN_ENV: &str = "SSHELF_TAILSCALE_BIN";

/// The macOS app bundle ships the CLI here and doesn't put it on `PATH`.
const MACOS_APP_BIN: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";

/// The slice of tailscale's `ipn/ipnstate.Status` we read. Unknown fields are ignored, so a
/// newer client only breaks us if it renames one of these.
#[derive(Debug, Deserialize)]
struct Status {
    #[serde(rename = "BackendState", default)]
    backend_state: String,
    #[serde(rename = "CurrentTailnet", default)]
    current_tailnet: Option<Tailnet>,
    /// Keyed by node public key. A `BTreeMap` keeps iteration (and therefore
    /// first-wins collision handling) deterministic.
    #[serde(rename = "Peer", default)]
    peer: Option<BTreeMap<String, PeerStatus>>,
}

#[derive(Debug, Default, Deserialize)]
struct Tailnet {
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "MagicDNSSuffix", default)]
    magic_dns_suffix: String,
    #[serde(rename = "MagicDNSEnabled", default)]
    magic_dns_enabled: bool,
}

/// One peer (`ipnstate.PeerStatus`). `Tags` and `Expired` are omitted when unset; the IP
/// lists are `null` rather than absent on a node that has none — hence the `Option`s.
#[derive(Debug, Deserialize)]
struct PeerStatus {
    #[serde(rename = "HostName", default)]
    host_name: String,
    /// The MagicDNS FQDN, with a trailing dot.
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Option<Vec<String>>,
    /// ACL tags, each prefixed `tag:`.
    #[serde(rename = "Tags", default)]
    tags: Option<Vec<String>>,
    #[serde(rename = "Expired", default)]
    expired: Option<bool>,
}

/// Resolve the user's `tailscale` binary, run `status --json`, and map the tailnet to hosts.
/// The one impure entry point of this module.
pub fn import_tailnet() -> Result<ImportResult> {
    let bin = resolve_binary()?;
    let json = capture_status(&bin)?;
    parse_status_json(&json)
}

/// Map `tailscale status --json` output to hosts. Pure: no process, no file, no socket.
///
/// A peer is imported when its `DNSName` sits under the tailnet's own `MagicDNSSuffix` —
/// one rule that excludes Mullvad exit nodes and foreign/shared-in nodes alike. Expired peers
/// are skipped; **offline peers are not** (`Online` is transient — an asleep laptop is still a
/// real host). `Self` isn't in the peer map, so it's never imported.
pub fn parse_status_json(text: &str) -> Result<ImportResult> {
    let status: Status = serde_json::from_str(text).context(
        "could not parse the output of `tailscale status --json` — a tailscale client much \
         newer or older than this build is the likely cause (check `tailscale version`)",
    )?;

    if status.backend_state != "Running" {
        let state = match status.backend_state.as_str() {
            "" => "unknown",
            s => s,
        };
        bail!(
            "tailscale isn't running (backend state: {state}) — run `tailscale up` (or start \
             the Tailscale app), then try the import again"
        );
    }

    let tailnet = status.current_tailnet.unwrap_or_default();
    // Compare suffixes case-insensitively and without the trailing dot of an FQDN.
    let suffix = tailnet.magic_dns_suffix.trim_matches('.').to_lowercase();
    if suffix.is_empty() {
        bail!(
            "`tailscale status --json` reported no tailnet (CurrentTailnet.MagicDNSSuffix is \
             empty) — check `tailscale status` and make sure this machine is fully logged in"
        );
    }
    let site = if tailnet.name.is_empty() {
        first_label(&suffix).to_string()
    } else {
        tailnet.name.clone()
    };

    let mut hosts: Vec<Host> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let (mut foreign, mut expired, mut unusable) = (0usize, 0usize, 0usize);

    for peer in status.peer.unwrap_or_default().values() {
        if peer.expired.unwrap_or(false) {
            expired += 1;
            continue;
        }
        let dns = peer.dns_name.trim().trim_end_matches('.');
        // An empty DNSName can't be checked against the suffix — but it also can't belong to
        // another tailnet (those always carry their own FQDN), so it falls back to HostName/IP.
        if !dns.is_empty() && !dns.to_lowercase().ends_with(&format!(".{suffix}")) {
            foreign += 1;
            continue;
        }
        let ips = peer.tailscale_ips.as_deref().unwrap_or_default();
        let (Some(name), Some(hostname)) = (
            peer_name(dns, &peer.host_name),
            peer_hostname(dns, ips, tailnet.magic_dns_enabled),
        ) else {
            unusable += 1;
            continue;
        };
        if !seen.insert(name.clone()) {
            warnings.push(format!(
                "two peers map to the name {name:?} — kept the first, skipped the other"
            ));
            continue;
        }
        let mut host = Host::new(name, hostname);
        host.site = Some(site.clone());
        host.tags = peer_tags(peer.tags.as_deref().unwrap_or_default());
        hosts.push(host);
    }
    // MagicDNS names are unique inside a tailnet, so this is a stable, tidy order.
    hosts.sort_by(|a, b| a.name.cmp(&b.name));

    let excluded = foreign + expired + unusable;
    if excluded > 0 {
        let mut parts = Vec::new();
        if foreign > 0 {
            parts.push(format!("{foreign} outside this tailnet"));
        }
        if expired > 0 {
            parts.push(format!("{expired} with an expired node key"));
        }
        if unusable > 0 {
            parts.push(format!("{unusable} with no usable name or address"));
        }
        warnings.insert(
            0,
            format!("{excluded} peer(s) excluded ({})", parts.join(", ")),
        );
    }
    Ok(ImportResult { hosts, warnings })
}

/// The host alias: the first label of the MagicDNS name (unique within a tailnet), or the
/// peer's `HostName` when it has no DNS name. Lowercased; `None` when both are empty.
fn peer_name(dns: &str, host_name: &str) -> Option<String> {
    let from_dns = first_label(dns).trim();
    let name = if from_dns.is_empty() {
        host_name.trim()
    } else {
        from_dns
    };
    (!name.is_empty()).then(|| name.to_lowercase())
}

/// What to connect to: the MagicDNS FQDN (stable across IP churn, and what Tailscale SSH
/// expects), falling back to the peer's Tailscale IP — IPv4 first — when MagicDNS is off for
/// the tailnet or the peer has no DNS name.
fn peer_hostname(dns: &str, ips: &[String], magic_dns_enabled: bool) -> Option<String> {
    if magic_dns_enabled && !dns.is_empty() {
        return Some(dns.to_string());
    }
    ips.iter()
        .find(|ip| ip.trim().parse::<Ipv4Addr>().is_ok())
        .or_else(|| ips.first())
        .map(|ip| ip.trim().to_string())
        .filter(|ip| !ip.is_empty())
        .or_else(|| (!dns.is_empty()).then(|| dns.to_string()))
}

/// ACL tags arrive as `tag:server`; sshelf tags are bare words. Peers without tags get an
/// empty list — no marker tag is invented.
fn peer_tags(tags: &[String]) -> Vec<String> {
    tags.iter()
        .map(|t| t.strip_prefix("tag:").unwrap_or(t).trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn first_label(fqdn: &str) -> &str {
    fqdn.split('.').next().unwrap_or("")
}

/// Locate the user's `tailscale` CLI: `$SSHELF_TAILSCALE_BIN`, then `PATH`, then (on macOS)
/// the app bundle. Errors name all three so the user knows what to fix.
pub fn resolve_binary() -> Result<PathBuf> {
    let env_bin = std::env::var_os(BIN_ENV).filter(|v| !v.is_empty());
    let path_var = std::env::var_os("PATH");
    resolve_in(
        env_bin.as_deref().map(Path::new),
        path_var.as_deref(),
        cfg!(target_os = "macos").then_some(Path::new(MACOS_APP_BIN)),
    )
}

/// The resolution rule over explicit inputs, so tests never touch the process environment.
fn resolve_in(
    env_bin: Option<&Path>,
    path_var: Option<&OsStr>,
    app_bin: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(bin) = env_bin {
        if !is_executable(bin) {
            bail!(
                "${BIN_ENV} is set to {} — that isn't an executable file",
                bin.display()
            );
        }
        return Ok(bin.to_path_buf());
    }
    if let Some(found) = path_var.and_then(|p| find_on_path(p, "tailscale")) {
        return Ok(found);
    }
    if let Some(app) = app_bin.filter(|p| is_executable(p)) {
        return Ok(app.to_path_buf());
    }
    bail!(
        "could not find the `tailscale` CLI. sshelf looks in three places:\n  \
         1. ${BIN_ENV} (not set)\n  \
         2. `tailscale` on your PATH\n  \
         3. {MACOS_APP_BIN} (the macOS app, which doesn't add its CLI to PATH)\n\
         Install the Tailscale CLI, or point sshelf at it with \
         {BIN_ENV}=/path/to/tailscale."
    )
}

fn find_on_path(path_var: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Run the user's `tailscale status --json` and return its stdout. The only process this
/// feature spawns, and only from `sshelf import --tailscale`.
fn capture_status(bin: &Path) -> Result<String> {
    let out = Command::new(bin)
        .args(["status", "--json"])
        .output()
        .with_context(|| format!("running `{} status --json`", bin.display()))?;
    let stdout = String::from_utf8(out.stdout)
        .with_context(|| format!("`{} status --json` returned invalid UTF-8", bin.display()))?;
    // Some states exit non-zero while still reporting a usable status document, so the exit
    // code only matters when there's nothing to parse.
    if stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = match stderr.trim() {
            "" => String::new(),
            msg => format!(": {msg}"),
        };
        bail!(
            "`{} status --json` produced no output ({}){detail}",
            bin.display(),
            out.status
        );
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::new_hosts;
    use crate::model::AuthMethod;

    /// Shaped after a real `tailscale status --json` (fields sshelf reads, plus a couple of
    /// neighbours for realism): a tagged peer, an untagged one, an offline one, an expired
    /// one, a Mullvad exit node, and a shared-in node from another tailnet.
    const TAILNET: &str = r#"{
  "Version": "1.98.9-t4fb758c39-g200941d74",
  "BackendState": "Running",
  "MagicDNSSuffix": "tail4f9a2.ts.net",
  "CurrentTailnet": {
    "Name": "homelab.example.com",
    "MagicDNSSuffix": "tail4f9a2.ts.net",
    "MagicDNSEnabled": true
  },
  "Self": {
    "HostName": "workstation",
    "DNSName": "workstation.tail4f9a2.ts.net.",
    "TailscaleIPs": ["100.64.0.1"],
    "Online": true
  },
  "Peer": {
    "nodekey:1111111111111111111111111111111111111111111111111111111111111111": {
      "ID": "n1",
      "HostName": "nas",
      "DNSName": "nas.tail4f9a2.ts.net.",
      "OS": "linux",
      "TailscaleIPs": ["100.64.0.11", "fd7a:115c:a1e0::1101"],
      "Tags": ["tag:server", "tag:storage"],
      "Online": true,
      "Active": true
    },
    "nodekey:2222222222222222222222222222222222222222222222222222222222222222": {
      "ID": "n2",
      "HostName": "raeds-macbook-pro",
      "DNSName": "raeds-macbook-pro.tail4f9a2.ts.net.",
      "OS": "macOS",
      "TailscaleIPs": ["100.64.0.12", "fd7a:115c:a1e0::1202"],
      "Online": true,
      "Active": false
    },
    "nodekey:3333333333333333333333333333333333333333333333333333333333333333": {
      "ID": "n3",
      "HostName": "Old-Laptop",
      "DNSName": "old-laptop.tail4f9a2.ts.net.",
      "OS": "windows",
      "TailscaleIPs": ["100.64.0.13"],
      "Online": false,
      "Active": false
    },
    "nodekey:4444444444444444444444444444444444444444444444444444444444444444": {
      "ID": "n4",
      "HostName": "retired-pi",
      "DNSName": "retired-pi.tail4f9a2.ts.net.",
      "OS": "linux",
      "TailscaleIPs": ["100.64.0.14"],
      "Expired": true,
      "Online": false
    },
    "nodekey:5555555555555555555555555555555555555555555555555555555555555555": {
      "ID": "n5",
      "HostName": "de-ber-wg-101",
      "DNSName": "de-ber-wg-101.mullvad.ts.net.",
      "OS": "linux",
      "TailscaleIPs": ["100.96.0.5"],
      "Tags": ["tag:mullvad-exit-node"],
      "ExitNodeOption": true,
      "Online": true
    },
    "nodekey:6666666666666666666666666666666666666666666666666666666666666666": {
      "ID": "n6",
      "HostName": "buildbox",
      "DNSName": "buildbox.tailc0ffee.ts.net.",
      "OS": "linux",
      "TailscaleIPs": ["100.72.0.9"],
      "Online": true
    }
  },
  "User": null,
  "ClientVersion": null
}"#;

    fn find<'a>(r: &'a ImportResult, name: &str) -> &'a Host {
        r.hosts
            .iter()
            .find(|h| h.name == name)
            .unwrap_or_else(|| panic!("expected a host named {name:?}"))
    }

    #[test]
    fn maps_eligible_peers_and_their_fields() {
        let r = parse_status_json(TAILNET).unwrap();
        let names: Vec<&str> = r.hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["nas", "old-laptop", "raeds-macbook-pro"]);

        let nas = find(&r, "nas");
        // MagicDNS FQDN, trailing dot trimmed — stable across IP churn.
        assert_eq!(nas.hostname, "nas.tail4f9a2.ts.net");
        assert_eq!(nas.tags, vec!["server".to_string(), "storage".to_string()]);
        assert_eq!(nas.site.as_deref(), Some("homelab.example.com"));
        assert_eq!(nas.auth, AuthMethod::Agent);
        assert_eq!(nas.user, None);
        assert_eq!(nas.port, None);
        assert!(nas.jump_hosts.is_empty() && nas.extra_args.is_none());
        assert!(!nas.id.is_empty());

        // An untagged peer gets no tags — no marker tag is invented.
        assert!(find(&r, "raeds-macbook-pro").tags.is_empty());
        // Offline is transient, so an asleep machine is still imported; the name is lowercased.
        assert_eq!(
            find(&r, "old-laptop").hostname,
            "old-laptop.tail4f9a2.ts.net"
        );
    }

    #[test]
    fn excludes_expired_and_foreign_peers_with_one_warning() {
        let r = parse_status_json(TAILNET).unwrap();
        for skipped in ["retired-pi", "de-ber-wg-101", "buildbox"] {
            assert!(
                !r.hosts.iter().any(|h| h.name == skipped),
                "{skipped} should not be imported"
            );
        }
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        let w = &r.warnings[0];
        assert!(w.starts_with("3 peer(s) excluded"), "{w}");
        assert!(w.contains("2 outside this tailnet"), "{w}");
        assert!(w.contains("1 with an expired node key"), "{w}");
    }

    #[test]
    fn self_is_never_imported() {
        let r = parse_status_json(TAILNET).unwrap();
        assert!(!r.hosts.iter().any(|h| h.name == "workstation"));
    }

    #[test]
    fn magic_dns_disabled_falls_back_to_the_ipv4_address() {
        let json = r#"{
          "BackendState": "Running",
          "CurrentTailnet": {
            "Name": "tail4f9a2.ts.net",
            "MagicDNSSuffix": "tail4f9a2.ts.net",
            "MagicDNSEnabled": false
          },
          "Peer": {
            "nodekey:aa": {
              "HostName": "nas",
              "DNSName": "nas.tail4f9a2.ts.net.",
              "TailscaleIPs": ["fd7a:115c:a1e0::1101", "100.64.0.11"]
            },
            "nodekey:bb": {
              "HostName": "v6only",
              "DNSName": "v6only.tail4f9a2.ts.net.",
              "TailscaleIPs": ["fd7a:115c:a1e0::1202"]
            }
          }
        }"#;
        let r = parse_status_json(json).unwrap();
        // The name still comes from MagicDNS; only the address falls back — IPv4 first,
        // whatever the order in TailscaleIPs, and the first entry when there is no IPv4.
        assert_eq!(find(&r, "nas").hostname, "100.64.0.11");
        assert_eq!(find(&r, "v6only").hostname, "fd7a:115c:a1e0::1202");
    }

    #[test]
    fn peer_without_a_dns_name_uses_hostname_and_ip() {
        let json = r#"{
          "BackendState": "Running",
          "CurrentTailnet": {
            "Name": "tail4f9a2.ts.net",
            "MagicDNSSuffix": "tail4f9a2.ts.net",
            "MagicDNSEnabled": true
          },
          "Peer": {
            "nodekey:aa": {
              "HostName": "Legacy-Box",
              "DNSName": "",
              "TailscaleIPs": ["100.64.0.21"]
            },
            "nodekey:bb": { "HostName": "", "DNSName": "", "TailscaleIPs": null }
          }
        }"#;
        let r = parse_status_json(json).unwrap();
        assert_eq!(r.hosts.len(), 1);
        assert_eq!(r.hosts[0].name, "legacy-box");
        assert_eq!(r.hosts[0].hostname, "100.64.0.21");
        // The nameless, address-less peer is counted, not silently dropped.
        assert!(
            r.warnings[0].contains("1 with no usable name or address"),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn colliding_names_keep_the_first_and_warn() {
        // Same first label under two sub-domains of the tailnet: only one host survives.
        let json = r#"{
          "BackendState": "Running",
          "CurrentTailnet": {
            "Name": "tail4f9a2.ts.net",
            "MagicDNSSuffix": "tail4f9a2.ts.net",
            "MagicDNSEnabled": true
          },
          "Peer": {
            "nodekey:aa": { "HostName": "nas", "DNSName": "nas.tail4f9a2.ts.net." },
            "nodekey:bb": { "HostName": "nas", "DNSName": "nas.eu.tail4f9a2.ts.net." }
          }
        }"#;
        let r = parse_status_json(json).unwrap();
        assert_eq!(r.hosts.len(), 1);
        // Peers iterate in public-key order, so the first key wins deterministically.
        assert_eq!(r.hosts[0].hostname, "nas.tail4f9a2.ts.net");
        assert!(
            r.warnings.iter().any(|w| w.contains("kept the first")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn tailnet_name_falls_back_to_the_suffixs_first_label() {
        let json = r#"{
          "BackendState": "Running",
          "CurrentTailnet": { "Name": "", "MagicDNSSuffix": "tail4f9a2.ts.net", "MagicDNSEnabled": true },
          "Peer": { "nodekey:aa": { "HostName": "nas", "DNSName": "nas.tail4f9a2.ts.net." } }
        }"#;
        let r = parse_status_json(json).unwrap();
        assert_eq!(r.hosts[0].site.as_deref(), Some("tail4f9a2"));
    }

    #[test]
    fn a_backend_that_isnt_running_says_how_to_fix_it() {
        for state in ["Stopped", "NeedsLogin", "NoState"] {
            let json = format!("{{\"BackendState\": \"{state}\", \"Peer\": null}}");
            let err = parse_status_json(&json).unwrap_err().to_string();
            assert!(err.contains(state), "{err}");
            assert!(err.contains("tailscale up"), "{err}");
        }
    }

    #[test]
    fn garbage_json_blames_the_tailscale_version() {
        let err = parse_status_json("not json at all").unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("tailscale status --json"), "{text}");
        assert!(text.contains("tailscale version"), "{text}");

        // Valid JSON of the wrong shape fails the same way.
        assert!(parse_status_json(r#"{"BackendState": 7}"#).is_err());
    }

    #[test]
    fn a_running_tailnet_with_no_suffix_is_an_error() {
        let json = r#"{"BackendState": "Running", "CurrentTailnet": null, "Peer": null}"#;
        let err = parse_status_json(json).unwrap_err().to_string();
        assert!(err.contains("no tailnet"), "{err}");
    }

    #[test]
    fn dedupe_against_existing_hosts_is_case_insensitive() {
        let r = parse_status_json(TAILNET).unwrap();
        let existing = vec![Host::new("NAS", "10.0.0.1")];
        let fresh = new_hosts(&r.hosts, &existing);
        assert!(!fresh.iter().any(|h| h.name.eq_ignore_ascii_case("nas")));
        assert_eq!(fresh.len(), 2);
        // Re-importing the same tailnet converges to nothing new.
        assert!(new_hosts(&r.hosts, &r.hosts).is_empty());
    }

    #[test]
    fn tags_lose_their_tag_prefix() {
        assert_eq!(
            peer_tags(&["tag:server".into(), "plain".into(), "tag:".into()]),
            vec!["server".to_string(), "plain".to_string()]
        );
        assert!(peer_tags(&[]).is_empty());
    }

    // --- binary resolution -------------------------------------------------------------

    /// A directory with an executable `tailscale` stub in it.
    fn stub_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sshelf-ts-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("tailscale");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }

    #[test]
    fn the_env_override_wins_over_path() {
        let dir = stub_dir();
        let explicit = dir.join("tailscale");
        let found = resolve_in(Some(&explicit), Some(OsStr::new("/nowhere")), None).unwrap();
        assert_eq!(found, explicit);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_bogus_env_override_is_reported_as_such() {
        let err = resolve_in(Some(Path::new("/nope/tailscale")), None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains(BIN_ENV), "{err}");
        assert!(err.contains("/nope/tailscale"), "{err}");
    }

    #[test]
    fn path_is_searched_then_the_macos_app_bundle() {
        let dir = stub_dir();
        let found = resolve_in(None, Some(dir.as_os_str()), None).unwrap();
        assert_eq!(found, dir.join("tailscale"));

        // Nothing on PATH → the app-bundle fallback, when it exists.
        let app = dir.join("tailscale");
        let found = resolve_in(None, Some(OsStr::new("/nowhere")), Some(&app)).unwrap();
        assert_eq!(found, app);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn not_found_anywhere_names_all_three_options() {
        let err = resolve_in(None, Some(OsStr::new("/nowhere")), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains(BIN_ENV), "{err}");
        assert!(err.contains("PATH"), "{err}");
        assert!(err.contains(MACOS_APP_BIN), "{err}");
    }
}
