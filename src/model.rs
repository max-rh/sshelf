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

/// Prefix for the secret-store key of a named [`Credential`]. Host ids are bare ULIDs
/// (Crockford base32, no colon), so a prefixed name can never collide with one — which is why
/// per-host secrets saved by earlier versions keep working untouched.
pub const CREDENTIAL_KEY_PREFIX: &str = "credential:";

/// One secret shared by any number of hosts, under a name the user picks.
///
/// The value still lives in the keyring or the vault, never here (same invariant as a host's
/// own password). This table only records that the name exists and where its value comes from,
/// so one directory password can serve a whole fleet and rotate in one place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    /// The name hosts reference (see [`Host::credential`]). Lower-case, `[a-z0-9._-]`.
    pub name: String,
    /// Where the value comes from. `None` means sshelf's own store (keyring or vault), which is
    /// the only source this version resolves; the field exists so an external source can be
    /// added without a schema change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// What this credential is for, shown in listings. Never the secret itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Credential {
    pub fn new(name: impl Into<String>) -> Self {
        Credential {
            name: name.into(),
            source: None,
            note: None,
        }
    }

    /// The key this credential's value is stored under.
    pub fn secret_key(&self) -> String {
        credential_secret_key(&self.name)
    }
}

/// The secret-store key for a credential name.
pub fn credential_secret_key(name: &str) -> String {
    format!("{CREDENTIAL_KEY_PREFIX}{}", name.to_ascii_lowercase())
}

/// Credential names are keys in the secret store (and in the macOS keychain's visible account
/// field), so they are kept to a predictable charset rather than accepting anything.
pub fn validate_credential_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a credential name cannot be empty".to_string());
    }
    if name.len() > 64 {
        return Err("a credential name is at most 64 characters".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "'{name}' has characters a credential name cannot use; allowed: letters, digits, \
             '-', '_' and '.'"
        ));
    }
    Ok(name.to_ascii_lowercase())
}

/// Case-insensitive lookup of a credential by name.
pub fn find_credential<'a>(creds: &'a [Credential], name: &str) -> Option<&'a Credential> {
    creds.iter().find(|c| c.name.eq_ignore_ascii_case(name))
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
    /// A bare site: name only, no shared defaults (what an import creates).
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
    /// The shared [`Credential`] this host's secret comes from, by name. When set, the secret
    /// is looked up under that name instead of this host's id, so several hosts answer from one
    /// stored value. A name with no `[[credential]]` entry still resolves (the table is
    /// metadata, not the lookup), so a hand-edited file degrades gracefully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The login needs an interactive second factor (a keyboard-interactive verification code,
    /// e.g. TOTP). When set, connect prompts for the code first and supplies it to `ssh` via the
    /// askpass helper. Required because a connect that auto-supplies a stored secret runs with
    /// `SSH_ASKPASS_REQUIRE=force`, which otherwise can't answer the code prompt. See D-022.
    #[serde(default, skip_serializing_if = "is_false")]
    pub requires_2fa: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
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
            credential: None,
            requires_2fa: false,
        }
    }

    /// The key this host's secret is stored under: its shared [`Credential`] when it has one,
    /// otherwise its own id. The only place that mapping is decided, so every caller (connect,
    /// `set-password`, first connect, `doctor`) agrees without passing the credential list
    /// around.
    pub fn secret_key(&self) -> String {
        match &self.credential {
            Some(name) => credential_secret_key(name),
            None => self.id.clone(),
        }
    }

    /// True when this host's secret is shared with other hosts rather than its own.
    pub fn shares_secret(&self) -> bool {
        self.credential.is_some()
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

    /// `user@host:port` for the host **as stored** — no site defaults resolved. Use
    /// [`Host::endpoint_in`] for anything a person reads.
    pub fn endpoint(&self) -> String {
        format!(
            "{}@{}:{}",
            self.effective_user(),
            self.hostname,
            self.port_or_default()
        )
    }

    /// `user@host:port` with the host's [`Site`] defaults resolved — what every human-facing
    /// listing shows, so an inherited user reads as the site's user rather than `$USER`.
    pub fn endpoint_in(&self, sites: &[Site]) -> String {
        self.with_site_defaults(sites).endpoint()
    }

    /// The haystack string used for fuzzy matching (name + endpoint + tags + site). Uses the
    /// site-resolved endpoint so typing a site's user finds the hosts that inherit it.
    pub fn search_haystack(&self, sites: &[Site]) -> String {
        let mut s = format!("{} {}", self.name, self.endpoint_in(sites));
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
    /// Shared secrets, by name. Declared before the hosts that reference them so the file
    /// reads top-down when hand-edited.
    #[serde(default, rename = "credential", skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<Credential>,
    #[serde(default, rename = "site", skip_serializing_if = "Vec::is_empty")]
    pub sites: Vec<Site>,
    #[serde(default, rename = "host", skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<Host>,
}

/// The version stamped on a file that uses nothing newer than the v1 feature set.
pub const CURRENT_FORMAT_VERSION: u32 = 1;

/// The version a file must declare once it uses shared credentials.
pub const CREDENTIALS_FORMAT_VERSION: u32 = 2;

/// The newest version this build can read. A file declaring more is refused on load rather
/// than parsed with its unknown parts dropped on the next save.
pub const MAX_SUPPORTED_FORMAT_VERSION: u32 = CREDENTIALS_FORMAT_VERSION;

impl Default for HostsFile {
    fn default() -> Self {
        HostsFile {
            format_version: CURRENT_FORMAT_VERSION,
            credentials: Vec::new(),
            sites: Vec::new(),
            hosts: Vec::new(),
        }
    }
}

impl HostsFile {
    /// A file holding exactly these parts. The `format_version` is stamped on save, from
    /// [`HostsFile::required_format_version`], so no caller has to pick one.
    pub fn with_parts(credentials: Vec<Credential>, sites: Vec<Site>, hosts: Vec<Host>) -> Self {
        HostsFile {
            format_version: CURRENT_FORMAT_VERSION,
            credentials,
            sites,
            hosts,
        }
    }

    /// The lowest `format_version` that describes what this file actually contains. A database
    /// with no shared credentials keeps declaring v1, so adopting 0.16 does not by itself make
    /// a file that an older sshelf would refuse.
    pub fn required_format_version(&self) -> u32 {
        if self.credentials.is_empty() && self.hosts.iter().all(|h| h.credential.is_none()) {
            CURRENT_FORMAT_VERSION
        } else {
            CREDENTIALS_FORMAT_VERSION
        }
    }

    /// Every credential name referenced by a host but missing from the `[[credential]]` table.
    /// These still resolve (the table is metadata), so this is a `doctor` warning, not an error.
    pub fn undeclared_credentials(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for name in self.hosts.iter().filter_map(|h| h.credential.as_deref()) {
            if find_credential(&self.credentials, name).is_none()
                && !out.iter().any(|n| n.eq_ignore_ascii_case(name))
            {
                out.push(name.to_string());
            }
        }
        out
    }

    /// Hosts bound to `name`, in file order.
    pub fn hosts_using(&self, name: &str) -> Vec<&Host> {
        self.hosts
            .iter()
            .filter(|h| {
                h.credential
                    .as_deref()
                    .is_some_and(|c| c.eq_ignore_ascii_case(name))
            })
            .collect()
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
        assert!(h.search_haystack(&[]).contains("prod-dc"));
    }

    #[test]
    fn endpoint_in_resolves_the_sites_user_and_port() {
        let sites = [prod_site()];
        let mut inherits = Host::new("web", "10.0.0.1");
        inherits.site = Some("prod".into());
        assert_eq!(inherits.endpoint_in(&sites), "deploy@10.0.0.1:2222");
        // The stored record is untouched, and `endpoint()` still shows it.
        assert_eq!(inherits.user, None);
        assert!(!inherits.endpoint().starts_with("deploy@"));
        // The haystack follows, so fuzzy-searching the inherited user finds the host.
        assert!(inherits.search_haystack(&sites).contains("deploy@"));

        // A host with its own user keeps it.
        let mut own = Host::new("db", "10.0.0.2");
        own.site = Some("prod".into());
        own.user = Some("mike".into());
        assert_eq!(own.endpoint_in(&sites), "mike@10.0.0.2:2222");
    }

    #[test]
    fn an_unbound_host_keeps_keying_its_secret_on_its_own_id() {
        let h = Host::new("web", "10.0.0.1");
        assert_eq!(h.secret_key(), h.id);
        assert!(!h.shares_secret());
        // The bare id is what every released version stored under, so nothing migrates.
        assert!(!h.secret_key().contains(':'));
    }

    #[test]
    fn a_bound_host_keys_its_secret_on_the_credential() {
        let mut h = Host::new("web", "10.0.0.1");
        h.credential = Some("Corp-LDAP".into());
        assert_eq!(h.secret_key(), "credential:corp-ldap");
        assert!(h.shares_secret());
        // Two hosts, one key: that is the whole point.
        let mut other = Host::new("db", "10.0.0.2");
        other.credential = Some("corp-ldap".into());
        assert_eq!(h.secret_key(), other.secret_key());
    }

    #[test]
    fn site_defaults_never_change_where_the_secret_lives() {
        let mut h = Host::new("web", "10.0.0.1");
        h.credential = Some("corp-ldap".into());
        h.site = Some("prod".into());
        let resolved = h.with_site_defaults(&[prod_site()]);
        assert_eq!(resolved.secret_key(), h.secret_key());
    }

    #[test]
    fn credential_names_are_checked_and_lowercased() {
        assert_eq!(
            validate_credential_name("  Corp-LDAP "),
            Ok("corp-ldap".into())
        );
        assert_eq!(validate_credential_name("a.b_c-1"), Ok("a.b_c-1".into()));
        assert!(validate_credential_name("").is_err());
        assert!(validate_credential_name("corp ldap").is_err(), "no spaces");
        assert!(validate_credential_name("corp:ldap").is_err(), "no colon");
        assert!(validate_credential_name("corp/ldap").is_err());
        assert!(validate_credential_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn a_credential_name_can_never_collide_with_a_host_id() {
        // A ULID has no colon, and the prefix always does.
        let h = Host::new("web", "10.0.0.1");
        assert_ne!(credential_secret_key(&h.id), h.id);
        assert!(credential_secret_key("anything").starts_with(CREDENTIAL_KEY_PREFIX));
    }

    #[test]
    fn the_format_version_rises_only_when_credentials_are_used() {
        let mut file =
            HostsFile::with_parts(Vec::new(), Vec::new(), vec![Host::new("w", "1.1.1.1")]);
        assert_eq!(file.required_format_version(), CURRENT_FORMAT_VERSION);

        // A bound host is enough on its own.
        file.hosts[0].credential = Some("corp".into());
        assert_eq!(file.required_format_version(), CREDENTIALS_FORMAT_VERSION);

        // So is a declaration with nothing bound to it yet.
        let declared = HostsFile::with_parts(vec![Credential::new("corp")], Vec::new(), Vec::new());
        assert_eq!(
            declared.required_format_version(),
            CREDENTIALS_FORMAT_VERSION
        );
    }

    #[test]
    fn a_used_but_undeclared_credential_is_reported_once() {
        let mut a = Host::new("a", "1.1.1.1");
        a.credential = Some("corp-ldap".into());
        let mut b = Host::new("b", "1.1.1.2");
        b.credential = Some("CORP-LDAP".into());
        let mut c = Host::new("c", "1.1.1.3");
        c.credential = Some("other".into());
        let file = HostsFile::with_parts(vec![Credential::new("other")], Vec::new(), vec![a, b, c]);

        assert_eq!(file.undeclared_credentials(), vec!["corp-ldap".to_string()]);
        // Case-insensitive on the way in, so both hosts count as users.
        assert_eq!(file.hosts_using("corp-ldap").len(), 2);
        assert_eq!(file.hosts_using("Other").len(), 1);
        assert_eq!(file.hosts_using("nobody").len(), 0);
    }
}
