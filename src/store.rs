//! Loading and saving the host database (`hosts.toml`) with crash-safe atomic writes.

use anyhow::{Context, Result, anyhow};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

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
    // A ULID rather than the pid — a pid is small and recycled, so the temp name was predictable
    // enough for anyone who can write to the directory to plant it before us.
    let tmp = parent.join(format!(".{}.tmp.{}", file_name, ulid::Ulid::new()));
    sweep_stale_temps(parent, file_name);

    // Scope the file handle so it's closed before the rename.
    {
        let mut f = create_exclusive(&tmp, mode)
            .with_context(|| format!("creating temp file {}", tmp.display()))?;
        f.write_all(bytes)
            .and_then(|()| f.sync_all())
            .inspect_err(|_| {
                let _ = fs::remove_file(&tmp);
            })?;
    }

    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp); // best-effort cleanup on failure
    })?;
    Ok(())
}

/// A temp file this old is nobody's in-flight write any more.
const STALE_TEMP_AGE: Duration = Duration::from_secs(60 * 60);

/// Best-effort removal of temp files an earlier run left next to `file_name`.
///
/// Every write picks a fresh ULID, so an orphan from a SIGKILL, a panic or a power cut is never
/// touched again by the writer — it would just sit in the user's hand-edited config directory
/// forever. Anything younger than [`STALE_TEMP_AGE`] is spared: it may belong to a write another
/// process is doing right now. Errors are ignored throughout; this is housekeeping, not the job.
fn sweep_stale_temps(parent: &Path, file_name: &str) {
    let prefix = format!(".{file_name}.tmp.");
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(&prefix) {
            continue;
        }
        // `DirEntry::metadata` does not follow symlinks, so a link planted here is judged (and
        // unlinked) on its own age, never its target's.
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age >= STALE_TEMP_AGE));
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Create `path` for writing, failing if *anything* already exists there.
///
/// `create_new` is what makes this safe: it refuses an existing file and — the point of the
/// exercise — refuses a symlink instead of following it to whatever it aims at. The mode goes to
/// `open(2)` itself, so the file is never briefly readable by others the way a create-then-chmod
/// leaves it while the umask decides. `open(2)` then applies `mode & ~umask`, which can only make
/// the file *narrower* than asked, so we fchmod the exact mode through the handle we already hold
/// — no second path lookup, so nothing to race. `mode` is unix permission bits, ignored on
/// non-unix.
pub fn create_exclusive(path: &Path, mode: u32) -> std::io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    Ok(file)
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

    #[test]
    fn atomic_write_sweeps_temp_files_a_crash_left_behind() {
        use std::time::{Duration, SystemTime};
        let dir = tmpdir();
        let path = dir.join("f.txt");

        // An orphan from a run that was killed mid-write: right name, hours old.
        let stale = dir.join(format!(".f.txt.tmp.{}", ulid::Ulid::new()));
        std::fs::write(&stale, "half a hosts file").unwrap();
        let handle = std::fs::File::options().write(true).open(&stale).unwrap();
        let long_ago = SystemTime::now() - Duration::from_secs(4 * 60 * 60);
        handle
            .set_times(std::fs::FileTimes::new().set_modified(long_ago))
            .unwrap();
        drop(handle);

        // A write another process may be in the middle of right now, and an unrelated dotfile.
        let fresh = dir.join(format!(".f.txt.tmp.{}", ulid::Ulid::new()));
        std::fs::write(&fresh, "in flight").unwrap();
        let other = dir.join(".other.toml.tmp.01ABC");
        std::fs::write(&other, "not ours").unwrap();

        atomic_write(&path, b"hello", 0o600).unwrap();

        assert!(!stale.exists(), "a stale temp file must be swept");
        assert!(
            fresh.exists(),
            "a temp file that may still be live is spared"
        );
        assert!(
            other.exists(),
            "another file's temp is none of our business"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    #[cfg(unix)]
    fn create_exclusive_refuses_a_symlink_and_spares_its_target() {
        let dir = tmpdir();
        let target = dir.join("target");
        std::fs::write(&target, "precious").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(create_exclusive(&link, 0o600).is_err());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "precious",
            "the symlink's target must not be written through"
        );
    }

    #[test]
    fn create_exclusive_refuses_an_existing_file() {
        let path = tmpdir().join("f");
        std::fs::write(&path, "already here").unwrap();
        assert!(create_exclusive(&path, 0o600).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "already here");
    }

    #[test]
    #[cfg(unix)]
    fn create_exclusive_applies_the_mode_at_creation() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmpdir().join("f");
        drop(create_exclusive(&path, 0o600).unwrap());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }
}
