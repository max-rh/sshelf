//! Encrypted-file secret store (the fallback when no OS keyring is used).
//!
//! A single `age` file holds a JSON map of `host_id -> password`, encrypted with a master
//! passphrase (age = scrypt KDF + ChaCha20-Poly1305). Used when `SSHELF_VAULT_PASSPHRASE`
//! is set — e.g. on headless Linux with no Secret Service daemon.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow};

use crate::store::atomic_write;

type Map = BTreeMap<String, String>;

fn encrypt(plaintext: &[u8], passphrase: &str) -> Result<Vec<u8>> {
    let encryptor =
        age::Encryptor::with_user_passphrase(age::secrecy::Secret::new(passphrase.to_owned()));
    let mut out = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut out)
        .context("age: wrap_output")?;
    writer.write_all(plaintext)?;
    writer.finish().context("age: finish")?;
    Ok(out)
}

fn decrypt(ciphertext: &[u8], passphrase: &str) -> Result<Vec<u8>> {
    let decryptor = match age::Decryptor::new(ciphertext).context("age: open vault")? {
        age::Decryptor::Passphrase(d) => d,
        _ => {
            return Err(anyhow!(
                "the vault is not a passphrase-encrypted age file — sshelf did not write it; \
                 move it aside and re-add your secrets"
            ));
        }
    };
    let mut reader = decryptor
        .decrypt(&age::secrecy::Secret::new(passphrase.to_owned()), None)
        .map_err(|_| {
            anyhow!(
                "could not decrypt the vault — is $SSHELF_VAULT_PASSPHRASE the passphrase it \
                 was created with?"
            )
        })?;
    let mut plaintext = Vec::new();
    reader.read_to_end(&mut plaintext)?;
    Ok(plaintext)
}

fn load_map(path: &Path, passphrase: &str) -> Result<Map> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let enc = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if enc.is_empty() {
        return Ok(Map::new());
    }
    let json = decrypt(&enc, passphrase)?;
    serde_json::from_slice(&json).context("parsing decrypted vault")
}

fn save_map(path: &Path, passphrase: &str, map: &Map) -> Result<()> {
    let json = serde_json::to_vec(map)?;
    let enc = encrypt(&json, passphrase)?;
    atomic_write(path, &enc, 0o600)
}

pub fn store(path: &Path, passphrase: &str, id: &str, password: &str) -> Result<()> {
    let mut map = load_map(path, passphrase)?;
    map.insert(id.to_string(), password.to_string());
    save_map(path, passphrase, &map)
}

pub fn get(path: &Path, passphrase: &str, id: &str) -> Result<Option<String>> {
    Ok(load_map(path, passphrase)?.get(id).cloned())
}

/// Every host id the vault holds a secret for. Read-only, and the reason `doctor` can report
/// orphaned secrets in vault mode: the vault is a map we own, unlike the OS keyring.
pub fn ids(path: &Path, passphrase: &str) -> Result<Vec<String>> {
    Ok(load_map(path, passphrase)?.into_keys().collect())
}

pub fn delete(path: &Path, passphrase: &str, id: &str) -> Result<()> {
    let mut map = load_map(path, passphrase)?;
    if map.remove(id).is_some() {
        save_map(path, passphrase, &map)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("sshelf-vault-{}.age", ulid::Ulid::new()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn store_get_roundtrip() {
        let path = tmp();
        store(&path, "master-pass", "id1", "s3cret").unwrap();
        store(&path, "master-pass", "id2", "other").unwrap();
        assert_eq!(
            get(&path, "master-pass", "id1").unwrap().as_deref(),
            Some("s3cret")
        );
        assert_eq!(
            get(&path, "master-pass", "id2").unwrap().as_deref(),
            Some("other")
        );
        assert_eq!(get(&path, "master-pass", "missing").unwrap(), None);
    }

    #[test]
    fn wrong_passphrase_fails() {
        let path = tmp();
        store(&path, "right", "id", "pw").unwrap();
        assert!(get(&path, "wrong", "id").is_err());
    }

    #[test]
    fn delete_removes_entry() {
        let path = tmp();
        store(&path, "p", "id", "pw").unwrap();
        delete(&path, "p", "id").unwrap();
        assert_eq!(get(&path, "p", "id").unwrap(), None);
    }

    #[test]
    fn ids_lists_every_stored_host() {
        let path = tmp();
        assert!(ids(&path, "p").unwrap().is_empty());
        store(&path, "p", "id2", "b").unwrap();
        store(&path, "p", "id1", "a").unwrap();
        let mut listed = ids(&path, "p").unwrap();
        listed.sort();
        assert_eq!(listed, vec!["id1".to_string(), "id2".to_string()]);
        // Listing is a read: a wrong passphrase can't fake an empty vault.
        assert!(ids(&path, "wrong").is_err());
    }

    #[test]
    fn missing_file_is_empty() {
        let path = tmp();
        assert_eq!(get(&path, "p", "id").unwrap(), None);
    }
}
