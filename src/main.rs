//! `sshelf` — a TUI SSH host manager.
//!
//! The binary runs in one of two modes:
//!  - normal: the interactive TUI (default) or a subcommand (`list`, `add`, `import`);
//!  - askpass: when invoked by `ssh` via `SSH_ASKPASS` (detected by the `SSHELF_ASKPASS`
//!    env var). Implemented in M5.

mod app;
mod askpass;
mod config;
mod import;
mod model;
mod paths;
mod search;
mod secrets;
mod ssh;
mod state;
mod store;
mod ui;
mod vault;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

use crate::config::Config;
use crate::paths::{CONFIG_ENV, Paths};
use crate::state::FrecencyState;
use anyhow::Context;

#[derive(Parser)]
#[command(
    name = "sshelf",
    version,
    about = "A TUI SSH host manager",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Use a specific config file (default: ~/.config/sshelf/config.toml).
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Connect directly to a saved host by name (skips the TUI). With no host and no
    /// subcommand, sshelf launches the interactive TUI.
    #[arg(value_name = "HOST")]
    host: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List saved hosts (sorted by frecency). Optional QUERY filters by fuzzy text and/or
    /// `tag:NAME` tokens — the same syntax as the TUI search box.
    List {
        #[arg(value_name = "QUERY", num_args = 0..)]
        query: Vec<String>,
    },
    /// Add a host via the wizard.
    Add,
    /// Import hosts from ~/.ssh/config (read-only).
    Import {
        /// Show what would be imported without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Store a password (read from stdin) for a host, by name or id.
    SetPassword {
        /// Host name or id.
        host: String,
    },
    /// Print the generated ssh command for a host without connecting.
    #[command(name = "print-command")]
    Print {
        /// Host name or id.
        host: String,
    },
    /// Print shell completions to stdout (for packaging / `source <(sshelf completions bash)`).
    Completions {
        /// Shell: bash, zsh, fish, elvish, or powershell.
        shell: clap_complete::Shell,
    },
    /// Print the man page (roff) to stdout.
    Man,
}

fn main() -> Result<()> {
    // ssh invokes us as `sshelf "<prompt>"` with SSHELF_ASKPASS=1 in the environment.
    // This must be checked before clap, since the prompt is a positional arg, not a flag.
    if std::env::var_os("SSHELF_ASKPASS").is_some() {
        let prompt = std::env::args().nth(1).unwrap_or_default();
        std::process::exit(askpass::run(&prompt));
    }

    let cli = Cli::parse();
    // A `--config` flag is plumbed to all paths via the env var (so subcommands and Paths
    // resolution see it uniformly). Set before any Paths::resolve().
    if let Some(path) = &cli.config {
        // SAFETY: set once at startup, before any threads are spawned.
        unsafe {
            std::env::set_var(CONFIG_ENV, path);
        }
    }
    match cli.command {
        Some(Command::List { query }) => cmd_list(&query.join(" ")),
        Some(Command::Add) => {
            println!("`sshelf add` arrives in M4. For now, edit hosts.toml directly.");
            Ok(())
        }
        Some(Command::Import { dry_run }) => cmd_import(dry_run),
        Some(Command::SetPassword { host }) => cmd_set_password(&host),
        Some(Command::Print { host }) => cmd_print_command(&host),
        Some(Command::Completions { shell }) => {
            clap_complete::generate(shell, &mut Cli::command(), "sshelf", &mut std::io::stdout());
            Ok(())
        }
        Some(Command::Man) => clap_mangen::Man::new(Cli::command())
            .render(&mut std::io::stdout())
            .context("rendering man page"),
        // No subcommand: a bare host name connects directly; otherwise launch the TUI.
        None => match cli.host {
            Some(name) => cmd_connect(&name),
            None => app::run(),
        },
    }
}

fn cmd_import(dry_run: bool) -> Result<()> {
    let path = import::default_config_path().context("HOME is not set")?;
    if !path.exists() {
        anyhow::bail!("no ssh config at {}", path.display());
    }
    let result = import::parse_file(&path)?;
    println!(
        "Parsed {} host(s) from {}",
        result.hosts.len(),
        path.display()
    );
    for w in &result.warnings {
        println!("  warning: {w}");
    }

    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let cfg = Config::load(&paths.config_file())?;
    let hosts_path = cfg.hosts_path(&paths);
    let mut file = store::load_hosts(&hosts_path)?;
    let to_add = import::new_hosts(&result.hosts, &file.hosts)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();

    if to_add.is_empty() {
        println!("Nothing new to import (all names already exist).");
        return Ok(());
    }
    println!("{} new host(s):", to_add.len());
    for h in &to_add {
        println!("  {:<20} {}", h.name, h.endpoint());
    }
    if dry_run {
        println!("(dry run — nothing written)");
        return Ok(());
    }
    file.hosts.extend(to_add);
    store::save_hosts(&hosts_path, &file)?;
    println!("Imported into {}", hosts_path.display());
    Ok(())
}

fn cmd_set_password(host_ref: &str) -> Result<()> {
    use std::io::BufRead;
    let paths = Paths::resolve()?;
    let cfg = Config::load(&paths.config_file())?;
    let hosts = store::load_hosts(&cfg.hosts_path(&paths))?.hosts;
    let host = hosts
        .iter()
        .find(|h| h.id == host_ref || h.name == host_ref)
        .with_context(|| format!("no host with name or id '{host_ref}'"))?;

    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading password from stdin")?;
    let password = line.trim_end_matches(['\n', '\r']);
    if password.is_empty() {
        anyhow::bail!("empty password; nothing stored");
    }
    secrets::store_password(&paths.vault_file(), &host.id, password)?;
    println!("stored password for \"{}\" ({})", host.name, host.id);
    Ok(())
}

fn cmd_print_command(host_ref: &str) -> Result<()> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let _ = Config::ensure_default_file(&paths.config_file()); // best-effort
    let cfg = Config::load(&paths.config_file())?;
    let hosts = store::load_hosts(&cfg.hosts_path(&paths))?.hosts;
    let host = resolve_host(&hosts, host_ref)
        .with_context(|| format!("no host with name or id '{host_ref}'"))?;
    println!("{}", ssh::command_string(host));
    Ok(())
}

fn cmd_list(query: &str) -> Result<()> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let _ = Config::ensure_default_file(&paths.config_file()); // best-effort
    let cfg = Config::load(&paths.config_file())?;
    let hosts_path = cfg.hosts_path(&paths);
    let hosts = store::load_hosts(&hosts_path)?.hosts;
    let st = FrecencyState::load(&paths.state_file())?;

    if hosts.is_empty() {
        println!("No hosts yet. Add one with `sshelf add`, or create:");
        println!("  {}", hosts_path.display());
        return Ok(());
    }

    let order = search::rank(&hosts, query, &st, cfg.decay_rate, cfg.default_sort);
    if order.is_empty() {
        println!("No hosts match '{}'.", query.trim());
        return Ok(());
    }
    for &i in &order {
        let h = &hosts[i];
        let tags = if h.tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", h.tags.join(", "))
        };
        println!(
            "{:<20}  {:<28}  {}{}",
            h.name,
            h.endpoint(),
            h.auth.as_str(),
            tags
        );
    }
    Ok(())
}

/// Connect directly to a host by name or id, skipping the TUI. Mirrors the TUI connect path:
/// record frecency BEFORE `exec()` (nothing runs after a successful exec), wire `SSH_ASKPASS`
/// only when a secret is stored, then `exec` ssh.
fn cmd_connect(host_ref: &str) -> Result<()> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let _ = Config::ensure_default_file(&paths.config_file()); // best-effort
    let cfg = Config::load(&paths.config_file())?;
    let hosts = store::load_hosts(&cfg.hosts_path(&paths))?.hosts;

    let Some(host) = resolve_host(&hosts, host_ref).cloned() else {
        let st = FrecencyState::load(&paths.state_file()).unwrap_or_default();
        let order = search::rank(&hosts, host_ref, &st, cfg.decay_rate, cfg.default_sort);
        if order.is_empty() {
            anyhow::bail!("no host named '{host_ref}' — run `sshelf list` to see your hosts");
        }
        let names: Vec<&str> = order
            .iter()
            .take(5)
            .map(|&i| hosts[i].name.as_str())
            .collect();
        anyhow::bail!(
            "no host named '{host_ref}' — did you mean: {}",
            names.join(", ")
        );
    };

    // Persist usage BEFORE exec() — nothing runs after a successful exec.
    let mut st = FrecencyState::load(&paths.state_file())?;
    st.record_use(&host.id);
    if let Err(e) = st.save(&paths.state_file()) {
        eprintln!("sshelf: warning: could not save state: {e:#}");
    }
    let has_secret = secrets::get_password(&paths.vault_file(), &host.id)
        .ok()
        .flatten()
        .is_some();
    // Replaces this process on success; returns only on failure.
    Err(ssh::exec_connect(&host, has_secret))
}

/// Find a host by exact name or id.
fn resolve_host<'a>(hosts: &'a [model::Host], reference: &str) -> Option<&'a model::Host> {
    hosts
        .iter()
        .find(|h| h.name == reference || h.id == reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn cli_routes_host_vs_subcommand() {
        // A known subcommand name wins.
        let c = Cli::try_parse_from(["sshelf", "list"]).unwrap();
        assert!(matches!(c.command, Some(Command::List { .. })));
        assert!(c.host.is_none());

        // A bare, unknown token is the host positional → direct connect.
        let c = Cli::try_parse_from(["sshelf", "prod-web"]).unwrap();
        assert!(c.command.is_none());
        assert_eq!(c.host.as_deref(), Some("prod-web"));

        // Nothing → TUI.
        let c = Cli::try_parse_from(["sshelf"]).unwrap();
        assert!(c.command.is_none() && c.host.is_none());
    }

    #[test]
    fn list_captures_query_tokens() {
        let c = Cli::try_parse_from(["sshelf", "list", "tag:web", "staging"]).unwrap();
        match c.command {
            Some(Command::List { query }) => assert_eq!(query, vec!["tag:web", "staging"]),
            _ => panic!("expected the list subcommand"),
        }
    }

    #[test]
    fn print_command_captures_host() {
        let c = Cli::try_parse_from(["sshelf", "print-command", "prod-web"]).unwrap();
        match c.command {
            Some(Command::Print { host }) => assert_eq!(host, "prod-web"),
            _ => panic!("expected the print-command subcommand"),
        }
    }

    #[test]
    fn global_config_works_with_a_host() {
        let c = Cli::try_parse_from(["sshelf", "--config", "/tmp/x.toml", "prod-web"]).unwrap();
        assert_eq!(c.host.as_deref(), Some("prod-web"));
        assert_eq!(
            c.config.as_deref(),
            Some(std::path::Path::new("/tmp/x.toml"))
        );
    }

    #[test]
    fn resolve_host_by_name_and_id() {
        let mut h = model::Host::new("prod-web", "10.0.0.1");
        h.id = "abc123".into();
        let hosts = vec![h];
        assert!(resolve_host(&hosts, "prod-web").is_some());
        assert!(resolve_host(&hosts, "abc123").is_some());
        assert!(resolve_host(&hosts, "missing").is_none());
    }
}
