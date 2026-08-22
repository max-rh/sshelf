//! Secret storage router.
//!
//! - If `SSHELF_VAULT_PASSPHRASE` is set → use the age-encrypted [`crate::vault`] (good for
//!   headless Linux / automation, and deterministic for testing).
//! - Otherwise → use the OS keyring (macOS Keychain, Linux Secret Service, Windows
//!   Credential Manager).
//!
//! Secrets are keyed by the stable host id. See `docs/security.md` for the threat model.

use std::path::Path;

use anyhow::{Context, Result};

const SERVICE: &str = "sshelf";
pub const VAULT_PASS_ENV: &str = "SSHELF_VAULT_PASSPHRASE";

/// Account name for `doctor`'s keyring round-trip. It shares the real `sshelf` **service** on
/// purpose — a probe under a different service could succeed where the real path fails (macOS
/// Keychain ACLs and Secret Service collections are per-item) — while the name itself can never
/// collide with a host, since host ids are ULIDs. Written and deleted within one call.
const PROBE_ACCOUNT: &str = "sshelf-doctor-probe";

/// Which store secrets are going to right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// macOS Keychain / Linux Secret Service.
    Keyring,
    /// The `age` vault, selected by `$SSHELF_VAULT_PASSPHRASE`.
    Vault,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Keyring => "OS keyring",
            Backend::Vault => "age vault",
        }
    }
}

/// The backend the next `store`/`get`/`delete` will use.
pub fn backend() -> Backend {
    match vault_passphrase() {
        Some(_) => Backend::Vault,
        None => Backend::Keyring,
    }
}

fn vault_passphrase() -> Option<String> {
    std::env::var(VAULT_PASS_ENV).ok().filter(|s| !s.is_empty())
}

/// Prove the configured backend actually works, without touching any real secret.
///
/// Vault mode **reads** the vault (which is what proves the passphrase decrypts it); a vault
/// that doesn't exist yet is fine — nothing has been stored. Keyring mode round-trips a
/// throwaway entry ([`PROBE_ACCOUNT`]) and deletes it again, the one write `doctor` performs.
pub fn probe(vault_path: &Path) -> Result<()> {
    if let Some(pass) = vault_passphrase() {
        crate::vault::ids(vault_path, &pass).map(|_| ())
    } else {
        let entry = keyring_entry(PROBE_ACCOUNT)?;
        entry
            .set_password("probe")
            .context("writing a probe entry to the OS keyring")?;
        let read = entry
            .get_password()
            .context("reading the probe entry back from the OS keyring");
        // Always clean up, even if the read failed.
        let _ = entry.delete_credential();
        match read?.as_str() {
            "probe" => Ok(()),
            _ => Err(anyhow::anyhow!(
                "the OS keyring returned a different value than was written"
            )),
        }
    }
}

/// Every host id that currently has a stored secret, or `Ok(None)` when the backend cannot be
/// listed. The OS keyring has no portable enumeration API (the `keyring` crate exposes lookup
/// by name only), so orphan detection is a vault-mode capability.
pub fn stored_ids(vault_path: &Path) -> Result<Option<Vec<String>>> {
    match vault_passphrase() {
        Some(pass) => crate::vault::ids(vault_path, &pass).map(Some),
        None => Ok(None),
    }
}

fn keyring_entry(id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, id).context("opening keyring entry")
}

pub fn store_password(vault_path: &Path, id: &str, password: &str) -> Result<()> {
    if let Some(pass) = vault_passphrase() {
        crate::vault::store(vault_path, &pass, id, password)
    } else {
        keyring_entry(id)?
            .set_password(password)
            .context("storing password in OS keyring")
    }
}

pub fn get_password(vault_path: &Path, id: &str) -> Result<Option<String>> {
    if let Some(pass) = vault_passphrase() {
        crate::vault::get(vault_path, &pass, id)
    } else {
        match keyring_entry(id)?.get_password() {
            Ok(p) => Ok(Some(p)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).context("reading password from OS keyring"),
        }
    }
}

pub fn delete_password(vault_path: &Path, id: &str) -> Result<()> {
    if let Some(pass) = vault_passphrase() {
        crate::vault::delete(vault_path, &pass, id)
    } else {
        match keyring_entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e).context("deleting password from OS keyring"),
        }
    }
}
