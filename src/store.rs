//! Loading and saving the host database (`hosts.toml`) with crash-safe atomic writes.

use anyhow::{Context, Result, anyhow};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::model::{HostsFile, MAX_SUPPORTED_FORMAT_VERSION};

/// Load the host database. A missing file yields an empty (default) database.
pub fn load_hosts(path: &Path) -> Result<HostsFile> {
    Ok(load_hosts_stamped(path)?.0)
}

/// Load the host database and the revision it was read at, for [`save_hosts_if_unchanged`].
///
/// `None` means the file did not exist, which is its own revision: a save may then create it,
/// but must not overwrite one that appeared in the meantime.
pub fn load_hosts_stamped(path: &Path) -> Result<(HostsFile, Option<Revision>)> {
    if !path.exists() {
        return Ok((HostsFile::default(), None));
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: HostsFile =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if parsed.format_version > MAX_SUPPORTED_FORMAT_VERSION {
        anyhow::bail!(
            "{} declares format_version {}, and this sshelf reads up to {}. Upgrade sshelf \
             rather than letting an older build rewrite the file, which would drop whatever it \
             does not understand.",
            path.display(),
            parsed.format_version,
            MAX_SUPPORTED_FORMAT_VERSION
        );
    }
    Ok((parsed, Some(Revision::of(&text))))
}

/// What a loaded `hosts.toml` looked like, so a later save can tell whether anything else
/// wrote to it in between. `atomic_write` prevents a torn file; this prevents a lost update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    len: u64,
    hash: u64,
}

impl Revision {
    fn of(text: &str) -> Self {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        Revision {
            len: text.len() as u64,
            hash: h.finish(),
        }
    }

    /// The revision `path` is at right now, or `None` when it does not exist.
    fn current(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(Some(Revision::of(&text)))
    }
}

/// Persist the host database atomically, stamping the `format_version` the contents need.
pub fn save_hosts(path: &Path, hosts: &HostsFile) -> Result<()> {
    let mut hosts = hosts.clone();
    hosts.format_version = hosts.required_format_version();
    let text = toml::to_string_pretty(&hosts).context("serializing hosts")?;
    atomic_write(path, text.as_bytes(), 0o600)
}

/// Save only if `path` is still at `expected` (from [`load_hosts_stamped`]).
///
/// Refuses instead of overwriting when another sshelf process edited the file since it was
/// read, which is the difference between "my change landed" and "my change replaced theirs".
pub fn save_hosts_if_unchanged(
    path: &Path,
    hosts: &HostsFile,
    expected: Option<&Revision>,
) -> Result<()> {
    let now = Revision::current(path)?;
    if now.as_ref() != expected {
        anyhow::bail!(
            "{} changed since it was read (another sshelf probably saved it), so this change \
             was not written. Reload and try again.",
            path.display()
        );
    }
    save_hosts(path, hosts)
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

        let hf = HostsFile::with_parts(Vec::new(), vec![site.clone()], vec![a.clone(), b.clone()]);

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
    fn a_newer_format_version_is_refused_rather_than_silently_downgraded() {
        let path = tmpdir().join("hosts.toml");
        std::fs::write(
            &path,
            format!(
                "format_version = {}\n\n[[host]]\nid = \"X\"\nname = \"w\"\nhostname = \"1.1.1.1\"\n",
                crate::model::MAX_SUPPORTED_FORMAT_VERSION + 1
            ),
        )
        .unwrap();
        let err = load_hosts(&path).unwrap_err().to_string();
        assert!(err.contains("Upgrade sshelf"), "{err}");
        // The file is left exactly as it was: nothing read it, nothing rewrote it.
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("format_version")
        );
    }

    #[test]
    fn a_credential_file_round_trips_and_stamps_version_2() {
        let path = tmpdir().join("hosts.toml");
        let mut h = crate::model::Host::new("web", "10.0.0.1");
        h.credential = Some("corp-ldap".into());
        let mut cred = crate::model::Credential::new("corp-ldap");
        cred.note = Some("the directory password".into());
        let hf = HostsFile::with_parts(vec![cred], Vec::new(), vec![h]);

        save_hosts(&path, &hf).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("format_version = 2"), "{text}");
        assert!(text.contains("[[credential]]"), "{text}");
        // No secret ever reaches the file, only the pointer.
        assert!(!text.contains("password ="), "{text}");

        let loaded = load_hosts(&path).unwrap();
        assert_eq!(loaded.credentials, hf.credentials);
        assert_eq!(loaded.hosts[0].credential.as_deref(), Some("corp-ldap"));
    }

    #[test]
    fn a_file_without_credentials_still_declares_version_1() {
        let path = tmpdir().join("hosts.toml");
        let hf = HostsFile::with_parts(
            Vec::new(),
            Vec::new(),
            vec![crate::model::Host::new("web", "10.0.0.1")],
        );
        save_hosts(&path, &hf).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("format_version = 1"), "{text}");
    }

    #[test]
    fn a_stale_save_is_refused_instead_of_replacing_someone_elses_edit() {
        let path = tmpdir().join("hosts.toml");
        let first = HostsFile::with_parts(
            Vec::new(),
            Vec::new(),
            vec![crate::model::Host::new("a", "1.1.1.1")],
        );
        save_hosts(&path, &first).unwrap();

        // This process reads it...
        let (mut mine, rev) = load_hosts_stamped(&path).unwrap();
        mine.hosts.push(crate::model::Host::new("mine", "2.2.2.2"));

        // ...another one saves in the meantime.
        let mut theirs = first.clone();
        theirs
            .hosts
            .push(crate::model::Host::new("theirs", "3.3.3.3"));
        save_hosts(&path, &theirs).unwrap();

        let err = save_hosts_if_unchanged(&path, &mine, rev.as_ref())
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed since it was read"), "{err}");
        let on_disk = load_hosts(&path).unwrap();
        assert!(on_disk.hosts.iter().any(|h| h.name == "theirs"));
        assert!(
            !on_disk.hosts.iter().any(|h| h.name == "mine"),
            "the stale write must not have landed"
        );

        // Re-reading and retrying works.
        let (mut again, rev) = load_hosts_stamped(&path).unwrap();
        again.hosts.push(crate::model::Host::new("mine", "2.2.2.2"));
        save_hosts_if_unchanged(&path, &again, rev.as_ref()).unwrap();
        assert_eq!(load_hosts(&path).unwrap().hosts.len(), 3);
    }

    #[test]
    fn a_first_save_expects_no_file_and_notices_one_appearing() {
        let path = tmpdir().join("hosts.toml");
        let (empty, rev) = load_hosts_stamped(&path).unwrap();
        assert!(rev.is_none(), "a missing file has no revision");

        // Someone creates it before we write.
        save_hosts(
            &path,
            &HostsFile::with_parts(
                Vec::new(),
                Vec::new(),
                vec![crate::model::Host::new("theirs", "3.3.3.3")],
            ),
        )
        .unwrap();
        assert!(save_hosts_if_unchanged(&path, &empty, rev.as_ref()).is_err());
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
