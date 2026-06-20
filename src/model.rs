//! The host data model and the on-disk `hosts.toml` shape.
//!
//! Invariant: **no secrets live here.** `auth = "password"` only records that a host
//! uses password auth; the actual password is stored in the keyring/vault (see `secrets`).

use serde::{Deserialize, Serialize};

/// How a host authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    /// Public-key auth with one or more identity files (`-i`).
    Key,
    /// Password auth; the secret is auto-supplied via the askpass helper.
    Password,
    /// Rely on a running ssh-agent (the default).
    #[default]
    Agent,
}

impl AuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMethod::Key => "key",
            AuthMethod::Password => "password",
            AuthMethod::Agent => "agent",
        }
    }
}

/// A named grouping that may also carry optional shared SSH defaults its member hosts inherit
/// at connect time. Every setting is optional — a bare site (name only) is pure grouping.
/// Auth is **not** inheritable; it stays per-host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Site {
    /// The site's name; hosts reference it by this (see `Host::site`).
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Default ProxyJump chain (the site's bastion).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jump_hosts: Vec<String>,
    /// Default identity files (only applied to key-auth members).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identity_files: Vec<String>,
}

impl Site {
    #[allow(dead_code)] // wired into the sites manager + `sshelf sites add` (M3/M4)
    pub fn new(name: impl Into<String>) -> Self {
        Site {
            name: name.into(),
            user: None,
            port: None,
            jump_hosts: Vec::new(),
            identity_files: Vec::new(),
        }
    }
}

/// Case-insensitive lookup of a site by name.
pub fn find_site<'a>(sites: &'a [Site], name: &str) -> Option<&'a Site> {
    sites.iter().find(|s| s.name.eq_ignore_ascii_case(name))
}

/// A single saved host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// Stable unique id (ULID). Keys both the secret store and frecency state, so a host
    /// can be renamed without losing its password or usage history.
    pub id: String,
    /// Display alias (what you search and see in the list).
    pub name: String,
    /// IP address or DNS name. Required.
    pub hostname: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,

    #[serde(default)]
    pub auth: AuthMethod,

    /// Identity files for `auth = key` (repeatable `-i`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identity_files: Vec<String>,
    /// ProxyJump chain (`-J a,b,c`). Key/agent auth only in v1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jump_hosts: Vec<String>,
    /// Free-form tags for filtering/grouping.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Raw extra args appended verbatim (shlex-split). Escape hatch for anything unmodeled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_args: Option<String>,
    /// The [`Site`] this host belongs to, by name. Groups the host and supplies optional
    /// inherited defaults; an undefined name degrades to pure grouping (no inheritance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
}

impl Host {
    /// Create a new host with a freshly generated id and sensible defaults.
    pub fn new(name: impl Into<String>, hostname: impl Into<String>) -> Self {
        Host {
            id: ulid::Ulid::new().to_string(),
            name: name.into(),
            hostname: hostname.into(),
            user: None,
            port: None,
            auth: AuthMethod::default(),
            identity_files: Vec::new(),
            jump_hosts: Vec::new(),
            tags: Vec::new(),
            extra_args: None,
            site: None,
        }
    }

    /// The user to connect as: the stored user, else `$USER`, else `"root"`.
    pub fn effective_user(&self) -> String {
        self.user
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "root".to_string())
    }

    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or(22)
    }

    /// `user@host:port` summary used in the list and previews.
    pub fn endpoint(&self) -> String {
        format!(
            "{}@{}:{}",
            self.effective_user(),
            self.hostname,
            self.port_or_default()
        )
    }

    /// The haystack string used for fuzzy matching (name + endpoint + tags + site).
    pub fn search_haystack(&self) -> String {
        let mut s = format!("{} {}", self.name, self.endpoint());
        if !self.tags.is_empty() {
            s.push(' ');
            s.push_str(&self.tags.join(" "));
        }
        if let Some(site) = &self.site {
            s.push(' ');
            s.push_str(site);
        }
        s
    }

    /// A clone with the host's [`Site`] defaults filled in **only where this host leaves a
    /// field unset** (the host always wins). Returns a plain clone when the host has no site,
    /// or names a site that isn't defined (graceful degradation — pure grouping). The `id` is
    /// preserved, so frecency/secrets still key correctly.
    pub fn with_site_defaults(&self, sites: &[Site]) -> Host {
        let mut h = self.clone();
        let Some(site) = self.site.as_deref().and_then(|n| find_site(sites, n)) else {
            return h;
        };
        if h.user.is_none() {
            h.user = site.user.clone();
        }
        if h.port.is_none() {
            h.port = site.port;
        }
        if h.jump_hosts.is_empty() {
            h.jump_hosts = site.jump_hosts.clone();
        }
        if h.identity_files.is_empty() {
            h.identity_files = site.identity_files.clone();
        }
        h
    }
}

/// The whole `hosts.toml` file. `format_version` (a scalar) is declared first so it serializes
/// before the `[[site]]` / `[[host]]` arrays (TOML requires scalars before array-of-tables);
/// `sites` comes before `hosts` so site definitions read at the top when hand-editing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostsFile {
    pub format_version: u32,
    #[serde(default, rename = "site", skip_serializing_if = "Vec::is_empty")]
    pub sites: Vec<Site>,
    #[serde(default, rename = "host", skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<Host>,
}

pub const CURRENT_FORMAT_VERSION: u32 = 1;

impl Default for HostsFile {
    fn default() -> Self {
        HostsFile {
            format_version: CURRENT_FORMAT_VERSION,
            sites: Vec::new(),
            hosts: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let h = Host::new("web", "10.0.0.1");
        assert_eq!(h.port_or_default(), 22);
        assert_eq!(h.auth, AuthMethod::Agent);
        assert!(!h.id.is_empty());
    }

    #[test]
    fn effective_user_prefers_explicit() {
        let mut h = Host::new("web", "10.0.0.1");
        h.user = Some("deploy".into());
        assert_eq!(h.effective_user(), "deploy");
    }

    #[test]
    fn auth_serializes_lowercase() {
        let json = serde_json::to_string(&AuthMethod::Password).unwrap();
        assert_eq!(json, "\"password\"");
    }

    fn prod_site() -> Site {
        Site {
            name: "prod".into(),
            user: Some("deploy".into()),
            port: Some(2222),
            jump_hosts: vec!["bastion".into()],
            identity_files: vec!["/k".into()],
        }
    }

    #[test]
    fn site_defaults_fill_only_unset_fields() {
        let sites = [prod_site()];

        // A host that sets nothing inherits everything; the id is preserved.
        let mut bare = Host::new("web", "10.0.0.1");
        bare.site = Some("PROD".into()); // case-insensitive match
        let eff = bare.with_site_defaults(&sites);
        assert_eq!(eff.user.as_deref(), Some("deploy"));
        assert_eq!(eff.port, Some(2222));
        assert_eq!(eff.jump_hosts, vec!["bastion".to_string()]);
        assert_eq!(eff.identity_files, vec!["/k".to_string()]);
        assert_eq!(eff.id, bare.id);

        // Per-host fields win; only the unset ones (here, port) inherit.
        let mut own = Host::new("db", "10.0.0.2");
        own.site = Some("prod".into());
        own.user = Some("mike".into());
        own.jump_hosts = vec!["own-jump".into()];
        let eff = own.with_site_defaults(&sites);
        assert_eq!(eff.user.as_deref(), Some("mike"));
        assert_eq!(eff.jump_hosts, vec!["own-jump".to_string()]);
        assert_eq!(eff.port, Some(2222));
    }

    #[test]
    fn no_site_or_undefined_site_is_a_plain_clone() {
        let sites = [prod_site()];
        // site = None
        assert_eq!(Host::new("a", "h").with_site_defaults(&sites).user, None);
        // names a site that isn't defined → no inheritance, no panic
        let mut dangling = Host::new("b", "h");
        dangling.site = Some("ghost".into());
        assert_eq!(dangling.with_site_defaults(&sites).user, None);
    }

    #[test]
    fn find_site_is_case_insensitive() {
        let sites = vec![Site::new("Prod-DC"), Site::new("staging")];
        assert!(find_site(&sites, "prod-dc").is_some());
        assert!(find_site(&sites, "PROD-DC").is_some());
        assert!(find_site(&sites, "missing").is_none());
    }

    #[test]
    fn haystack_includes_site() {
        let mut h = Host::new("web", "10.0.0.1");
        h.site = Some("prod-dc".into());
        assert!(h.search_haystack().contains("prod-dc"));
    }
}
