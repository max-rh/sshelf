//! Loading and saving the host database (`hosts.toml`) with crash-safe atomic writes.

use anyhow::{Context, Result, anyhow};
use std::fs;
use std::io::Write;
use std::path::Path;

use crate::model::HostsFile;

/// Load the host database. A missing file yields an empty (default) database.
pub fn load_hosts(path: &Path) -> Result<HostsFile> {
    if !path.exists() {
        return Ok(HostsFile::default());
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: HostsFile =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(parsed)
}

/// Persist the host database atomically.
pub fn save_hosts(path: &Path, hosts: &HostsFile) -> Result<()> {
    let text = toml::to_string_pretty(hosts).context("serializing hosts")?;
    atomic_write(path, text.as_bytes(), 0o600)
}

/// Write `bytes` to `path` atomically: write a sibling temp file, fsync it, then rename
/// over the target. A crash mid-write leaves the previous file intact. `mode` is the
/// final unix permission bits (ignored on non-unix).
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent directory: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("invalid file name: {}", path.display()))?;
    let tmp = parent.join(format!(".{}.tmp.{}", file_name, std::process::id()));

    // Scope the file handle so it's closed before the rename.
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("creating temp file {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
    }

    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp); // best-effort cleanup on failure
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AuthMethod, Host, HostsFile};

    fn tmpdir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("sshelf-test-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_file_is_empty_db() {
        let p = tmpdir().join("hosts.toml");
        let hf = load_hosts(&p).unwrap();
        assert!(hf.hosts.is_empty());
        assert_eq!(hf.format_version, crate::model::CURRENT_FORMAT_VERSION);
    }

    #[test]
    fn round_trip_preserves_hosts() {
        let dir = tmpdir();
        let path = dir.join("hosts.toml");

        let mut a = Host::new("prod-db", "10.25.25.25");
        a.user = Some("mike".into());
        a.auth = AuthMethod::Key;
        a.identity_files = vec!["~/.ssh/infra-key".into()];
        a.tags = vec!["prod".into(), "db".into()];

        let mut b = Host::new("bastion", "bastion.example.com");
        b.port = Some(2222);
        b.auth = AuthMethod::Password;

        let mut site = crate::model::Site::new("prod-dc");
        site.user = Some("deploy".into());
        site.jump_hosts = vec!["bastion".into()];
        a.site = Some("prod-dc".into());

        let hf = HostsFile {
            format_version: crate::model::CURRENT_FORMAT_VERSION,
            sites: vec![site.clone()],
            hosts: vec![a.clone(), b.clone()],
        };

        save_hosts(&path, &hf).unwrap();
        let loaded = load_hosts(&path).unwrap();
        assert_eq!(loaded, hf);
        assert_eq!(loaded.sites[0], site);
        assert_eq!(loaded.hosts[0], a);
        assert_eq!(loaded.hosts[1], b);
    }

    #[test]
    fn loads_pre_sites_file_without_a_sites_array() {
        // An old hosts.toml (no [[site]], no host `site=`) must still load: sites default empty.
        let dir = tmpdir();
        let path = dir.join("hosts.toml");
        std::fs::write(
            &path,
            "format_version = 1\n\n[[host]]\nid = \"01HOST\"\nname = \"web\"\nhostname = \"10.0.0.1\"\n",
        )
        .unwrap();
        let loaded = load_hosts(&path).unwrap();
        assert!(loaded.sites.is_empty());
        assert_eq!(loaded.hosts.len(), 1);
        assert_eq!(loaded.hosts[0].site, None);
    }

    #[test]
    fn atomic_write_replaces_existing() {
        let dir = tmpdir();
        let path = dir.join("f.txt");
        atomic_write(&path, b"first", 0o600).unwrap();
        atomic_write(&path, b"second", 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        // no temp files left behind
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind");
    }
}
