//! Cross-platform path resolution.
//!
//! We deliberately use the XDG **base** strategy on every platform (via `etcetera::Xdg`),
//! so config lives in `~/.config/sshelf` on macOS *and* Linux (honoring `XDG_CONFIG_HOME`),
//! instead of being buried in macOS `~/Library`. This keeps the files hand-editable.

use anyhow::{Context, Result};
use etcetera::base_strategy::{BaseStrategy, Xdg};
use std::path::{Path, PathBuf};

/// Env var (or `--config`) pointing at a specific `config.toml` to use instead of the default.
pub const CONFIG_ENV: &str = "SSHELF_CONFIG";
const APP_DIR: &str = "sshelf";

pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    /// Explicit config-file path from `$SSHELF_CONFIG` / `--config`, if any.
    pub config_file_override: Option<PathBuf>,
}

impl Paths {
    pub fn resolve() -> Result<Self> {
        let xdg = Xdg::new().context("could not determine home directory")?;
        let data_dir = xdg.data_dir().join(APP_DIR);
        match std::env::var_os(CONFIG_ENV).filter(|s| !s.is_empty()) {
            // A custom config file: its parent becomes the config dir (used for default hosts).
            Some(cfg) => {
                let config_file = expand_user_path(&cfg.to_string_lossy());
                let config_dir = config_file
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                Ok(Paths {
                    config_dir,
                    data_dir,
                    config_file_override: Some(config_file),
                })
            }
            None => Ok(Paths {
                config_dir: xdg.config_dir().join(APP_DIR),
                data_dir,
                config_file_override: None,
            }),
        }
    }

    /// User-owned host database (default location; may be overridden by `config.hosts_file`).
    pub fn hosts_file(&self) -> PathBuf {
        self.config_dir.join("hosts.toml")
    }

    /// User preferences.
    pub fn config_file(&self) -> PathBuf {
        self.config_file_override
            .clone()
            .unwrap_or_else(|| self.config_dir.join("config.toml"))
    }

    /// App-owned frecency state.
    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join("state.json")
    }

    /// App-owned ledger of active background port-forwards.
    pub fn forwards_file(&self) -> PathBuf {
        self.data_dir.join("forwards.json")
    }

    /// Encrypted secret vault (fallback when no OS keyring is available).
    #[allow(dead_code)] // used by the vault backend (M5)
    pub fn vault_file(&self) -> PathBuf {
        self.data_dir.join("vault.age")
    }

    /// The exported ssh_config `Include` fragment (`sshelf export`). Lives next to the config,
    /// not under `~/.ssh` — the user references it from their own config with one Include line.
    pub fn ssh_config_file(&self) -> PathBuf {
        self.config_dir.join("ssh_config")
    }

    /// The default hosts-file path as a display string (for the settings placeholder).
    pub fn default_hosts_display(&self) -> String {
        self.hosts_file().display().to_string()
    }

    /// Create the config and data directories with restrictive perms.
    ///
    /// The default directories are ours alone, so they are created *and* held at 0700 every run.
    /// A custom config file (`--config` / `$SSHELF_CONFIG`) is different: its parent is a
    /// directory the user chose, quite possibly shared with other tools or other people, and
    /// `~/notes/sshelf.toml` must not make `~/notes` private behind their back. So we create it
    /// 0700 when it is missing and never touch the mode of one that already exists.
    pub fn ensure_dirs(&self) -> Result<()> {
        ensure_dir(&self.config_dir, self.config_file_override.is_none())?;
        ensure_dir(&self.data_dir, true)
    }
}

/// Create `dir` at mode 0700; when `enforce_mode`, also re-apply 0700 to a directory that was
/// already there. Missing *parents* are created at the process default instead.
///
/// Only the last component is ours. `--config /srv/team/sshelf/config.toml` on a box where
/// `/srv/team` does not exist yet must not hand `/srv/team` to sshelf's own umask — that is the
/// same mistake as chmodding a directory the user already had, one level up. So parents go
/// through plain `create_dir_all` and only the leaf gets the private mode.
///
/// A symlinked directory is followed on purpose. Dotfile managers routinely make `~/.config` (or
/// a directory under it) a symlink into a checkout, and refusing to write through one would break
/// an entirely ordinary setup for no real gain — anyone who can repoint that symlink can already
/// read the files it leads to.
pub(crate) fn ensure_private_dir(dir: &Path, enforce_mode: bool) -> std::io::Result<()> {
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        // mkdir(0o700) rather than create-then-chmod: a directory we create is never briefly
        // traversable by others. `mkdir(2)` applies `mode & ~umask`, which can only make it
        // *narrower*, so the chmod below restores exactly 0700 — but only on a directory we
        // made ourselves this run, never on one that was already sitting there.
        let created = match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if !dir.is_dir() {
                    return Err(std::io::Error::other(format!(
                        "{} exists but is not a directory",
                        dir.display()
                    )));
                }
                false
            }
            Err(e) => return Err(e),
        };
        if created || enforce_mode {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = enforce_mode;
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

fn ensure_dir(dir: &Path, enforce_mode: bool) -> Result<()> {
    ensure_private_dir(dir, enforce_mode).with_context(|| format!("creating {}", dir.display()))
}

/// Expand a leading `~` / `~/` to `$HOME`. Used for user-provided paths (config/hosts files).
pub fn expand_user_path(s: &str) -> PathBuf {
    if s == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tilde() {
        // SAFETY: single-threaded test.
        unsafe {
            std::env::set_var("HOME", "/home/tester");
        }
        assert_eq!(
            expand_user_path("~/x/y.toml"),
            PathBuf::from("/home/tester/x/y.toml")
        );
        assert_eq!(expand_user_path("/abs/p"), PathBuf::from("/abs/p"));
    }

    /// A fresh empty directory to hang test paths off. No process env is touched — the tests set
    /// the override field directly, so they stay safe to run in parallel.
    fn scratch() -> PathBuf {
        let p = std::env::temp_dir().join(format!("sshelf-paths-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[cfg(unix)]
    fn mode_of(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    #[cfg(unix)]
    fn default_dirs_are_created_private() {
        let root = scratch();
        let paths = Paths {
            config_dir: root.join("config/sshelf"),
            data_dir: root.join("data/sshelf"),
            config_file_override: None,
        };
        paths.ensure_dirs().unwrap();
        assert_eq!(mode_of(&paths.config_dir), 0o700);
        assert_eq!(mode_of(&paths.data_dir), 0o700);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    #[cfg(unix)]
    fn an_existing_custom_config_dir_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch();
        let shared = root.join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();

        let paths = Paths {
            config_dir: shared.clone(),
            data_dir: root.join("data"),
            config_file_override: Some(shared.join("sshelf.toml")),
        };
        paths.ensure_dirs().unwrap();
        assert_eq!(
            mode_of(&shared),
            0o755,
            "a directory sshelf did not create must not be chmodded"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    #[cfg(unix)]
    fn a_missing_ancestor_of_a_custom_config_dir_is_left_alone() {
        let root = scratch();
        // `--config <root>/shared/sshelf/config.toml` where neither directory exists yet.
        let ancestor = root.join("shared");
        let dir = ancestor.join("sshelf");
        let paths = Paths {
            config_dir: dir.clone(),
            data_dir: root.join("data"),
            config_file_override: Some(dir.join("config.toml")),
        };
        paths.ensure_dirs().unwrap();

        // What `mkdir -p` would have produced here, whatever this process's umask is.
        let reference = root.join("reference");
        std::fs::create_dir(&reference).unwrap();

        assert_eq!(mode_of(&dir), 0o700, "the config dir itself is ours");
        assert_eq!(
            mode_of(&ancestor),
            mode_of(&reference),
            "an ancestor sshelf merely had to pass through must keep the default mode"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    #[cfg(unix)]
    fn a_missing_custom_config_dir_is_created_private() {
        let root = scratch();
        let dir = root.join("new");
        let paths = Paths {
            config_dir: dir.clone(),
            data_dir: root.join("data"),
            config_file_override: Some(dir.join("sshelf.toml")),
        };
        paths.ensure_dirs().unwrap();
        assert_eq!(mode_of(&dir), 0o700);
        std::fs::remove_dir_all(&root).ok();
    }
}
