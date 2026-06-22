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

    /// The default hosts-file path as a display string (for the settings placeholder).
    pub fn default_hosts_display(&self) -> String {
        self.hosts_file().display().to_string()
    }

    /// Create the config and data directories with restrictive perms.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [&self.config_dir, &self.data_dir] {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("chmod 700 {}", dir.display()))?;
            }
        }
        Ok(())
    }
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
}
