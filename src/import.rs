//! Read-only import from `~/.ssh/config`. We never write back to it.
//!
//! Maps literal `Host` aliases to our model (name, hostname, user, port, identity files).
//! `Match`/`Include`/`ProxyJump` are not supported by the parser / our model — we scan for
//! them and warn so the user can fill those in manually rather than silently losing them.

use std::collections::HashSet;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ssh2_config::{ParseRule, SshConfig};

use crate::model::{AuthMethod, Host, Site, find_site};

/// What an importer produces, whatever its source (`~/.ssh/config`, a tailnet, …): the hosts
/// it mapped, plus what it couldn't map and wants the user to know about.
#[derive(Debug)]
pub struct ImportResult {
    pub hosts: Vec<Host>,
    pub warnings: Vec<String>,
}

/// `~/.ssh/config`, if `HOME` is set.
pub fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".ssh/config"))
}

pub fn parse_file(path: &Path) -> Result<ImportResult> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_str(&text)
}

pub fn parse_str(text: &str) -> Result<ImportResult> {
    let warnings = scan_unsupported(text);

    let mut reader = BufReader::new(text.as_bytes());
    let config = SshConfig::default()
        .parse(&mut reader, ParseRule::ALLOW_UNKNOWN_FIELDS)
        .context("parsing ssh config")?;

    let mut hosts = Vec::new();
    let mut seen = HashSet::new();
    for h in config.get_hosts() {
        for clause in &h.pattern {
            if clause.negated || is_wildcard(&clause.pattern) {
                continue;
            }
            let alias = clause.pattern.clone();
            if !seen.insert(alias.clone()) {
                continue;
            }
            let p = &h.params;
            let hostname = p.host_name.clone().unwrap_or_else(|| alias.clone());
            let mut host = Host::new(alias, hostname);
            host.user = p.user.clone();
            host.port = p.port;
            if let Some(ids) = &p.identity_file {
                let ids: Vec<String> = ids.iter().map(|pb| pb.display().to_string()).collect();
                if !ids.is_empty() {
                    host.auth = AuthMethod::Key;
                    host.identity_files = ids;
                }
            }
            hosts.push(host);
        }
    }
    Ok(ImportResult { hosts, warnings })
}

/// Parsed hosts whose name isn't already present in `existing` (case-insensitive).
pub fn new_hosts<'a>(parsed: &'a [Host], existing: &[Host]) -> Vec<&'a Host> {
    let names: HashSet<String> = existing.iter().map(|h| h.name.to_lowercase()).collect();
    parsed
        .iter()
        .filter(|h| !names.contains(&h.name.to_lowercase()))
        .collect()
}

/// Add-only site handling for imports: point each host at the **existing** spelling of the
/// site it names (matched case-insensitively) and return the bare sites that still need
/// creating. Existing sites are never modified — only referenced.
pub fn missing_sites(hosts: &mut [Host], existing: &[Site]) -> Vec<Site> {
    let mut fresh: Vec<Site> = Vec::new();
    for host in hosts.iter_mut() {
        let Some(name) = host.site.clone() else {
            continue;
        };
        // Clone the matched spelling before touching `fresh` again (it may be borrowed from it).
        let known = find_site(existing, &name)
            .or_else(|| find_site(&fresh, &name))
            .map(|site| site.name.clone());
        match known {
            Some(spelling) => host.site = Some(spelling),
            None => fresh.push(Site::new(name)),
        }
    }
    fresh
}

fn is_wildcard(pat: &str) -> bool {
    pat.contains('*') || pat.contains('?')
}

fn scan_unsupported(text: &str) -> Vec<String> {
    let (mut matches, mut includes, mut jumps) = (0u32, 0u32, 0u32);
    for line in text.lines() {
        let l = line.trim().to_lowercase();
        if l == "match" || l.starts_with("match ") {
            matches += 1;
        } else if l.starts_with("include ") {
            includes += 1;
        } else if l.starts_with("proxyjump ") || l.starts_with("proxyjump=") {
            jumps += 1;
        }
    }
    let mut w = Vec::new();
    if matches > 0 {
        w.push(format!("{matches} `Match` block(s) ignored (unsupported)"));
    }
    if includes > 0 {
        w.push(format!(
            "{includes} `Include` directive(s) ignored (unsupported)"
        ));
    }
    if jumps > 0 {
        w.push(format!(
            "{jumps} `ProxyJump` value(s) not imported — add jump hosts manually"
        ));
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Host prod-web
    HostName 10.0.0.1
    User deploy
    Port 2222
    IdentityFile ~/.ssh/infra-key

Host bastion
    HostName bastion.example.com
    User ops

Host *.internal
    User admin

Host behind
    HostName 10.0.5.7
    ProxyJump bastion
";

    fn find<'a>(r: &'a ImportResult, name: &str) -> &'a Host {
        r.hosts
            .iter()
            .find(|h| h.name == name)
            .expect("host present")
    }

    #[test]
    fn maps_basic_fields() {
        let r = parse_str(SAMPLE).unwrap();
        let web = find(&r, "prod-web");
        assert_eq!(web.hostname, "10.0.0.1");
        assert_eq!(web.user.as_deref(), Some("deploy"));
        assert_eq!(web.port, Some(2222));
        assert_eq!(web.auth, AuthMethod::Key);
        // ssh2-config expands ~ to an absolute path (fine — unambiguous to store).
        assert_eq!(web.identity_files.len(), 1);
        assert!(
            web.identity_files[0].ends_with(".ssh/infra-key"),
            "got {:?}",
            web.identity_files
        );
    }

    #[test]
    fn skips_wildcard_patterns() {
        let r = parse_str(SAMPLE).unwrap();
        assert!(!r.hosts.iter().any(|h| h.name.contains('*')));
        assert!(r.hosts.iter().any(|h| h.name == "bastion"));
    }

    #[test]
    fn warns_about_proxyjump() {
        let r = parse_str(SAMPLE).unwrap();
        assert!(r.warnings.iter().any(|w| w.contains("ProxyJump")));
    }

    #[test]
    fn missing_sites_matches_case_insensitively_and_creates_once() {
        let mut hosts = vec![
            Host::new("a", "h1"),
            Host::new("b", "h2"),
            Host::new("c", "h3"),
            Host::new("d", "h4"),
        ];
        hosts[0].site = Some("TAILNET".into()); // existing, differently cased
        hosts[1].site = Some("new-net".into()); // to be created
        hosts[2].site = Some("New-Net".into()); // same new site, differently cased
        // hosts[3] has no site at all

        let existing = vec![Site::new("Tailnet")];
        let fresh = missing_sites(&mut hosts, &existing);

        // The existing site is reused with its own spelling, never redefined.
        assert_eq!(hosts[0].site.as_deref(), Some("Tailnet"));
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0], Site::new("new-net")); // bare: name only, no defaults
        assert_eq!(hosts[1].site.as_deref(), Some("new-net"));
        assert_eq!(hosts[2].site.as_deref(), Some("new-net"));
        assert_eq!(hosts[3].site, None);
    }

    #[test]
    fn new_hosts_dedupes_by_name() {
        let r = parse_str(SAMPLE).unwrap();
        let existing = vec![Host::new("bastion", "x")];
        let fresh = new_hosts(&r.hosts, &existing);
        assert!(!fresh.iter().any(|h| h.name == "bastion"));
        assert!(fresh.iter().any(|h| h.name == "prod-web"));
    }
}
